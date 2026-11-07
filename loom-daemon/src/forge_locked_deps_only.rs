//! `loom-daemon forge locked-deps-only <pr>` (2am#6942 ruling, 2026-11-07) — the fail-closed
//! decision behind Champion's opt-in `lockedDepsOnly` standing-authorization class key, cloned
//! from `additiveMigrations` (#13102).
//!
//! # What the question is
//!
//! A `Cargo.toml` is a critical-file hit, and the suspect-line carve-out (#12999) holds any
//! change to a dependency table. The operator ruled that a new workspace-member crate whose
//! dependencies are all already in `Cargo.lock` should not hold. A class that sees only the
//! critical-file hits cannot tell, because `Cargo.lock` is not one. This verb reads both: does
//! the PR add **no third-party package** and no source-redirecting manifest key?
//!
//! # Rules (all must hold)
//!
//! 1. Every `Cargo.lock` in the PR is `added`/`modified` with a patch, and its patch has no
//!    added `source =` / `checksum =` line (a new registry or git package) and no removed
//!    `name =` / `version =` / `source =` / `checksum =` line (an existing package changed or
//!    dropped, i.e. a bump). Added workspace-member entries carry none of these.
//! 2. Every `Cargo.toml` is `added`/`modified` with a patch, and no changed line names `git =`,
//!    `registry =`, `build =`, `links =`, `[patch…]`, `[replace…]`, or a `path =` that is not a
//!    quoted path staying inside the repo (resolved from the manifest's directory). The head
//!    file must also hold no `[patch…]`/`[replace…]` table at all, since a hunk may sit under
//!    a header it does not show.
//! 3. At least one `Cargo.toml` and at least one `Cargo.lock` change: a manifest change with no
//!    lock change has no lock proof, so it holds.
//!
//! Any read or parse failure is a hold, and a caller treats **any** non-zero exit (clap's 2 on
//! a daemon predating this verb, 126/127 for a missing binary) as a hold.

use std::path::Path;

use anyhow::Result;
use regex::Regex;
use serde::Deserialize;

use crate::forge_additive_migrations::{decode_contents, gh_head_sha, safe_path};

/// `per_page` for the file-list read (GitHub's maximum).
const PAGE_SIZE: usize = 100;

/// GitHub's `pulls/<n>/files` stops at 3000 files, i.e. 30 full pages.
const MAX_PAGES: u32 = 30;

/// Why a PR is NOT cleared. Every variant exits 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// `--repo` was not a plain `OWNER/REPO`.
    InvalidRepo,
    /// A forge read failed or did not parse.
    FetchFailed(String),
    /// The file list may be truncated (the forge caps it).
    ListTruncated,
    /// The PR changes no `Cargo.toml`.
    NoManifest,
    /// The PR changes a manifest but no `Cargo.lock`: no lock proof.
    NoLockChange,
    /// A manifest/lock that is not a plain added or modified file.
    BadStatus { file: String, status: String },
    /// The file has no (or an empty) patch.
    NoPatch(String),
    /// The lockfile gains a registry/git package or changes an existing one.
    Lock { file: String, line: String },
    /// A manifest line that redirects a source or runs code.
    Manifest { file: String, line: String },
    /// A `path =` that is not a quoted, in-repo path.
    PathOutside { file: String, line: String },
    /// The head manifest holds a `[patch]`/`[replace]` table.
    PatchTable(String),
    /// A patch line that is not a hunk header or ` `/`+`/`-` line.
    Unparseable { file: String, line: String },
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRepo => f.write_str("--repo is not a plain OWNER/REPO"),
            Self::FetchFailed(why) => write!(f, "forge read failed: {why}"),
            Self::ListTruncated => f.write_str("the PR's file list may be truncated"),
            Self::NoManifest => f.write_str("the PR changes no Cargo.toml"),
            Self::NoLockChange => f.write_str("a Cargo.toml changed but no Cargo.lock did"),
            Self::BadStatus { file, status } => {
                write!(f, "{file} is not a plain added/modified file (status: {status})")
            }
            Self::NoPatch(file) => write!(f, "{file} has no patch (fails closed)"),
            Self::Lock { file, line } => {
                write!(f, "{file} adds a non-workspace package or changes one: {line}")
            }
            Self::Manifest { file, line } => write!(f, "{file} has a source/build key: {line}"),
            Self::PathOutside { file, line } => {
                write!(f, "{file} has a path outside the repo (or not a literal): {line}")
            }
            Self::PatchTable(file) => write!(f, "{file} holds a [patch]/[replace] table"),
            Self::Unparseable { file, line } => write!(f, "{file} patch line unparseable: {line}"),
        }
    }
}

/// One entry of a `pulls/<n>/files` page.
#[derive(Deserialize)]
struct FileEntry {
    filename: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    patch: Option<String>,
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Iterate a patch's changed lines as `(prefix, content)`, erroring on a line it cannot read.
fn changed_lines<'a>(file: &str, patch: &'a str) -> Result<Vec<(char, &'a str)>, Reason> {
    let mut out = Vec::new();
    let mut in_hunk = false;
    for line in patch.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk || line.is_empty() || line.starts_with('\\') {
            continue;
        }
        match line.chars().next() {
            Some(' ') => {}
            Some(c @ ('+' | '-')) => out.push((c, &line[1..])),
            _ => {
                return Err(Reason::Unparseable {
                    file: file.to_string(),
                    line: line.to_string(),
                })
            }
        }
    }
    Ok(out)
}

/// Rule 1: a `Cargo.lock` patch adds no third-party package and changes no existing one.
fn check_lock(file: &str, patch: &str) -> Result<(), Reason> {
    #[allow(clippy::unwrap_used)] // constant patterns
    let (added_bad, removed_bad) = (
        Regex::new(r"^(source|checksum)\s*=").unwrap(),
        Regex::new(r"^(name|version|source|checksum)\s*=").unwrap(),
    );
    for (p, content) in changed_lines(file, patch)? {
        let bad = if p == '+' {
            added_bad.is_match(content)
        } else {
            removed_bad.is_match(content)
        };
        if bad {
            return Err(Reason::Lock {
                file: file.to_string(),
                line: format!("{p}{content}"),
            });
        }
    }
    Ok(())
}

/// Resolve `rel` against `dir` (both repo-relative); `None` when it climbs above the root or is
/// not a plain relative path.
fn stays_in_repo(dir: &str, rel: &str) -> bool {
    if rel.is_empty()
        || rel.starts_with('/')
        || rel.starts_with('~')
        || rel.contains(':')
        || rel.contains('\\')
    {
        return false;
    }
    let mut depth: i64 = dir.split('/').filter(|s| !s.is_empty()).count() as i64;
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => depth += 1,
        }
    }
    true
}

/// The `[patch…]` / `[replace…]` table header test, shared by changed lines and the head file.
fn is_patch_header(re: &Regex, line: &str) -> bool {
    re.is_match(line)
}

/// Rule 2 on the changed lines of one `Cargo.toml` patch.
fn check_manifest(file: &str, patch: &str) -> Result<(), Reason> {
    #[allow(clippy::unwrap_used)] // constant patterns
    let (re_build, re_src, re_path, re_patch, re_patch_key) = (
        Regex::new(r"^\s*(build|links)\s*=").unwrap(),
        Regex::new(r"(^|[{,])\s*(git|registry)\s*=").unwrap(),
        Regex::new(r#"(?:^|[{,])\s*path\s*=\s*(?:"([^"]*)"|'([^']*)')?"#).unwrap(),
        Regex::new(r"^\s*\[\[?\s*(patch|replace)\b").unwrap(),
        Regex::new(r#"^\s*["']?(patch|replace)["']?\s*[.=]"#).unwrap(),
    );
    let dir = file.rsplit_once('/').map_or("", |(d, _)| d);
    for (p, content) in changed_lines(file, patch)? {
        let line = format!("{p}{content}");
        if re_build.is_match(content)
            || re_src.is_match(content)
            || is_patch_header(&re_patch, content)
            || re_patch_key.is_match(content)
        {
            return Err(Reason::Manifest {
                file: file.to_string(),
                line,
            });
        }
        for c in re_path.captures_iter(content) {
            let value = c.get(1).or_else(|| c.get(2)).map(|m| m.as_str());
            if !value.is_some_and(|v| stays_in_repo(dir, v)) {
                return Err(Reason::PathOutside {
                    file: file.to_string(),
                    line,
                });
            }
        }
    }
    Ok(())
}

/// Does the head manifest hold a `[patch…]` / `[replace…]` table?
fn head_has_patch_table(text: &str) -> bool {
    #[allow(clippy::unwrap_used)] // constant pattern
    let re = Regex::new(r"^\s*\[\[?\s*(patch|replace)\b").unwrap();
    text.lines().any(|l| re.is_match(l))
}

/// Decide a whole PR from its file list; `head` reads a manifest's text at the PR head.
fn decide_files<R>(entries: &[FileEntry], mut head: R) -> Result<(), Reason>
where
    R: FnMut(&str) -> Result<String, String>,
{
    let (mut manifests, mut locks) = (0usize, 0usize);
    for e in entries {
        let is_toml = basename(&e.filename) == "Cargo.toml";
        let is_lock = basename(&e.filename) == "Cargo.lock";
        if !is_toml && !is_lock {
            continue;
        }
        if e.status != "added" && e.status != "modified" {
            return Err(Reason::BadStatus {
                file: e.filename.clone(),
                status: if e.status.is_empty() {
                    "unknown".into()
                } else {
                    e.status.clone()
                },
            });
        }
        let patch = e
            .patch
            .as_deref()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| Reason::NoPatch(e.filename.clone()))?;
        if is_lock {
            locks += 1;
            check_lock(&e.filename, patch)?;
        } else {
            manifests += 1;
            check_manifest(&e.filename, patch)?;
            let text = head(&e.filename).map_err(Reason::FetchFailed)?;
            if head_has_patch_table(&text) {
                return Err(Reason::PatchTable(e.filename.clone()));
            }
        }
    }
    if manifests == 0 {
        return Err(Reason::NoManifest);
    }
    if locks == 0 {
        return Err(Reason::NoLockChange);
    }
    Ok(())
}

/// Walk the file list page by page through `fetch_page` (1-based page to raw JSON body).
fn list_files<F>(mut fetch_page: F) -> Result<Vec<FileEntry>, Reason>
where
    F: FnMut(u32) -> Result<Vec<u8>, String>,
{
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let body = fetch_page(page).map_err(Reason::FetchFailed)?;
        let entries: Vec<FileEntry> = serde_json::from_slice(&body)
            .map_err(|e| Reason::FetchFailed(format!("page {page} did not parse: {e}")))?;
        let full = entries.len() >= PAGE_SIZE;
        all.extend(entries);
        if !full {
            return Ok(all);
        }
    }
    Err(Reason::ListTruncated)
}

/// The whole verdict with the forge reads injected.
///
/// # Errors
///
/// The [`Reason`] the PR is not cleared.
pub fn decide<F, R>(fetch_page: F, head: R) -> Result<(), Reason>
where
    F: FnMut(u32) -> Result<Vec<u8>, String>,
    R: FnMut(&str) -> Result<String, String>,
{
    decide_files(&list_files(fetch_page)?, head)
}

fn gh_read(gh_bin: &Path, path: &str) -> Result<Vec<u8>, String> {
    crate::verdict_equivalence::gh_api_checked("forge.locked_deps_only", gh_bin, None, path)
}

/// Parsed `forge locked-deps-only` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// Pull request number.
    pub pr: u32,
    /// `OWNER/REPO`; `None` resolves it from the cwd (never a `{owner}/{repo}` placeholder).
    pub repo: Option<String>,
}

/// Handle `loom-daemon forge locked-deps-only <pr> [--repo OWNER/REPO]`.
/// Never returns (exits): 0 with `LOCKED_DEPS_ONLY=1` when the PR adds no third-party package,
/// otherwise 1 with the reason on stderr.
pub fn handle(args: &Args) -> Result<()> {
    use crate::forge_version_only_diff::{gh_fetch_page, resolve_repo, valid_repo};
    let pr = args.pr;
    let verdict = match args.repo.as_deref() {
        Some(r) if !valid_repo(r) => Err(Reason::InvalidRepo),
        r => {
            let gh = crate::forge_cmd::gh_bin();
            match resolve_repo(r, &gh) {
                Err(why) => Err(Reason::FetchFailed(why)),
                Ok(repo) => {
                    let repo = repo.as_str();
                    let gh = Path::new(&gh);
                    let mut sha: Option<String> = None;
                    decide(
                        |page| gh_fetch_page(gh, repo, pr, page),
                        |file| {
                            if !safe_path(file) {
                                return Err("unsafe file path".into());
                            }
                            if sha.is_none() {
                                sha = Some(gh_head_sha(gh, repo, pr)?);
                            }
                            let sha = sha.as_deref().unwrap_or_default();
                            decode_contents(&gh_read(
                                gh,
                                &format!("repos/{repo}/contents/{file}?ref={sha}"),
                            )?)
                        },
                    )
                }
            }
        }
    };
    match verdict {
        Ok(()) => {
            println!("LOCKED_DEPS_ONLY=1");
            std::process::exit(0);
        }
        Err(reason) => {
            eprintln!("loom-daemon forge locked-deps-only: PR #{pr} held: {reason}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn entry(name: &str, status: &str, patch: Option<&str>) -> FileEntry {
        FileEntry {
            filename: name.into(),
            status: status.into(),
            patch: patch.map(str::to_string),
        }
    }

    const NEW_CRATE_TOML: &str = "@@ -0,0 +1,12 @@\n+[package]\n+name = \"telemetry-port\"\n+version = \"0.1.0\"\n+edition = \"2021\"\n+\n+[dependencies]\n+reqwest = { version = \"0.12\", features = [\"json\"] }\n+serde = { version = \"1\", features = [\"derive\"] }\n+serde_json = \"1\"\n+sha2 = \"0.10\"\n+shared = { path = \"../shared\" }\n";
    /// The 2am#6943 lock shape: only the workspace crate's own entry, no `source =`.
    const WORKSPACE_LOCK: &str = "@@ -2100,6 +2100,18 @@ dependencies = [\n \n [[package]]\n+name = \"telemetry-port\"\n+version = \"0.1.0\"\n+dependencies = [\n+ \"reqwest\",\n+ \"serde\",\n+ \"serde_json\",\n+ \"sha2\",\n+]\n+\n+[[package]]\n name = \"tempfile\"\n version = \"3.10.1\"\n";
    const REGISTRY_LOCK: &str = "@@ -10,0 +10,8 @@\n+[[package]]\n+name = \"newcrate\"\n+version = \"1.0.0\"\n+source = \"registry+https://github.com/rust-lang/crates.io-index\"\n+checksum = \"abc\"\n";
    const BUMP_LOCK: &str = "@@ -10,5 +10,5 @@\n [[package]]\n name = \"serde\"\n-version = \"1.0.1\"\n+version = \"1.0.2\"\n source = \"registry+x\"\n";

    fn head_ok(_: &str) -> Result<String, String> {
        Ok("[package]\nname = \"x\"\n".into())
    }

    fn run(files: &[FileEntry]) -> Result<(), Reason> {
        decide_files(files, head_ok)
    }

    #[test]
    fn workspace_member_add_passes() {
        let f = [
            entry("infra/telemetry-port/Cargo.toml", "added", Some(NEW_CRATE_TOML)),
            entry("Cargo.lock", "modified", Some(WORKSPACE_LOCK)),
            entry("infra/telemetry-port/src/main.rs", "added", Some("@@ -0,0 +1 @@\n+fn main(){}")),
        ];
        assert_eq!(run(&f), Ok(()));
    }

    #[test]
    fn new_registry_crate_fails() {
        let f = [
            entry("infra/x/Cargo.toml", "added", Some(NEW_CRATE_TOML)),
            entry("Cargo.lock", "modified", Some(REGISTRY_LOCK)),
        ];
        assert!(matches!(run(&f), Err(Reason::Lock { .. })));
    }

    #[test]
    fn locked_crate_version_bump_fails() {
        let f = [
            entry(
                "infra/x/Cargo.toml",
                "modified",
                Some("@@ -1 +1 @@\n-serde = \"1.0.1\"\n+serde = \"1.0.2\""),
            ),
            entry("Cargo.lock", "modified", Some(BUMP_LOCK)),
        ];
        assert!(matches!(run(&f), Err(Reason::Lock { .. })));
    }

    #[test]
    fn git_dependency_fails() {
        let toml =
            "@@ -0,0 +1,2 @@\n+[dependencies]\n+foo = { git = \"https://example.com/foo\" }\n";
        let f = [
            entry("infra/x/Cargo.toml", "added", Some(toml)),
            entry("Cargo.lock", "modified", Some(WORKSPACE_LOCK)),
        ];
        assert!(matches!(run(&f), Err(Reason::Manifest { .. })));
    }

    #[test]
    fn manifest_change_without_lock_change_fails() {
        let f = [entry("infra/x/Cargo.toml", "added", Some(NEW_CRATE_TOML))];
        assert_eq!(run(&f), Err(Reason::NoLockChange));
    }

    #[test]
    fn registry_build_links_patch_replace_fail() {
        for line in [
            "foo = { version = \"1\", registry = \"alt\" }",
            "build = \"build.rs\"",
            "links = \"z\"",
            "[patch.crates-io]",
            "[replace]",
            "patch.crates-io.foo = { path = \"x\" }",
        ] {
            let toml = format!("@@ -0,0 +1 @@\n+{line}\n");
            let f = [
                entry("infra/x/Cargo.toml", "modified", Some(&toml)),
                entry("Cargo.lock", "modified", Some(WORKSPACE_LOCK)),
            ];
            assert!(matches!(run(&f), Err(Reason::Manifest { .. })), "{line}");
        }
    }

    #[test]
    fn path_outside_repo_or_non_literal_fails() {
        for line in [
            "a = { path = \"/etc/x\" }",
            "a = { path = \"../../../x\" }",
            "a = { path = \"~/x\" }",
            "a = { path = SOMETHING }",
        ] {
            let toml = format!("@@ -0,0 +1 @@\n+{line}\n");
            let f = [
                entry("infra/x/Cargo.toml", "modified", Some(&toml)),
                entry("Cargo.lock", "modified", Some(WORKSPACE_LOCK)),
            ];
            assert!(matches!(run(&f), Err(Reason::PathOutside { .. })), "{line}");
        }
        // `..` that stays inside the repo is fine.
        assert!(stays_in_repo("infra/x", "../shared"));
        assert!(!stays_in_repo("infra/x", "../../.."));
        assert!(!stays_in_repo("", "../x"));
    }

    #[test]
    fn head_patch_table_fails() {
        let f = [
            entry("Cargo.toml", "modified", Some("@@ -1 +1 @@\n-members = []\n+members = [\"a\"]")),
            entry("Cargo.lock", "modified", Some(WORKSPACE_LOCK)),
        ];
        let r = decide_files(&f, |_| {
            Ok("[workspace]\n[patch.crates-io]\nfoo = { path = \"v\" }\n".into())
        });
        assert_eq!(r, Err(Reason::PatchTable("Cargo.toml".into())));
    }

    #[test]
    fn multiple_lockfiles_each_checked() {
        let f = [
            entry("a/Cargo.toml", "added", Some(NEW_CRATE_TOML)),
            entry("a/Cargo.lock", "modified", Some(WORKSPACE_LOCK)),
            entry("b/Cargo.lock", "modified", Some(REGISTRY_LOCK)),
        ];
        assert!(matches!(run(&f), Err(Reason::Lock { .. })));
    }

    #[test]
    fn missing_patch_removed_or_renamed_fail_closed() {
        let ok_lock = entry("Cargo.lock", "modified", Some(WORKSPACE_LOCK));
        let f = [entry("a/Cargo.toml", "added", None), ok_lock];
        assert!(matches!(run(&f), Err(Reason::NoPatch(_))));
        let ok_lock = entry("Cargo.lock", "modified", Some(WORKSPACE_LOCK));
        let f = [
            entry("a/Cargo.toml", "removed", Some("@@ -1 +0,0 @@\n-x")),
            ok_lock,
        ];
        assert!(matches!(run(&f), Err(Reason::BadStatus { .. })));
        let f = [
            entry("a/Cargo.toml", "added", Some(NEW_CRATE_TOML)),
            entry("Cargo.lock", "renamed", Some(WORKSPACE_LOCK)),
        ];
        assert!(matches!(run(&f), Err(Reason::BadStatus { .. })));
    }

    #[test]
    fn no_manifest_and_unreadable_head_fail() {
        let f = [entry("Cargo.lock", "modified", Some(WORKSPACE_LOCK))];
        assert_eq!(run(&f), Err(Reason::NoManifest));
        let f = [
            entry("a/Cargo.toml", "added", Some(NEW_CRATE_TOML)),
            entry("Cargo.lock", "modified", Some(WORKSPACE_LOCK)),
        ];
        assert!(matches!(decide_files(&f, |_| Err("boom".into())), Err(Reason::FetchFailed(_))));
    }

    #[test]
    fn unparseable_patch_line_fails() {
        let f = [
            entry("a/Cargo.toml", "added", Some("@@ -0,0 +1 @@\n?weird")),
            entry("Cargo.lock", "modified", Some(WORKSPACE_LOCK)),
        ];
        assert!(matches!(run(&f), Err(Reason::Unparseable { .. })));
    }

    #[test]
    fn list_fetch_errors_hold() {
        assert!(matches!(decide(|_| Err("down".into()), head_ok), Err(Reason::FetchFailed(_))));
        assert!(matches!(decide(|_| Ok(b"nope".to_vec()), head_ok), Err(Reason::FetchFailed(_))));
    }

    #[test]
    fn full_list_at_cap_holds() {
        let page: Vec<String> = (0..PAGE_SIZE)
            .map(|i| format!(r#"{{"filename":"src/f{i}.rs","status":"modified"}}"#))
            .collect();
        let page = format!("[{}]", page.join(","));
        assert_eq!(decide(|_| Ok(page.clone().into_bytes()), head_ok), Err(Reason::ListTruncated));
    }
}
