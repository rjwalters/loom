//! Tree invariant (Issue #9985): every `gh` spawn in this crate goes through
//! the one choke point, `crate::gh_invocation`.
//!
//! # What counts as a raw site
//!
//! Outside `src/gh_invocation/`, any non-comment line matching one of:
//!
//! - `Command::new("gh")` — a literal program name;
//! - `Command::new(gh…)` — a `gh` / `gh_bin…` variable, field
//!   (`&self.gh_bin`), or call (`gh_bin()`), with optional `&` / `*` and an
//!   optional path prefix (`crate::gh_invocation::gh_bin()` — #10089: the
//!   resolver call spelled with its module path used to slip past the scan);
//! - `fn gh_bin…` — a private resolver (`gh_bin`, `gh_bin_env`,
//!   `gh_bin_or_default`, …).
//!
//! Word boundaries keep `Command::new(&ghost)` and `fn gh_binary_ok` style
//! near-misses honest: `gh` must be the whole identifier, and `gh_bin` may only
//! be followed by `_suffix` or a non-identifier character.
//!
//! # The ratchet
//!
//! Migration is spread over several PRs (#9985's slicing plan), so the test
//! does not demand zero today. Instead `tests/fixtures/gh-spawn-allowlist.txt`
//! records every raw site that existed when the scan landed, as
//! `<path> | <trimmed line>` (no line numbers, so unrelated edits do not churn
//! it). The scan must match the allowlist **exactly**, as a multiset:
//!
//! - a site the allowlist does not name fails — route it through
//!   `GhInvocation` instead of adding an entry;
//! - an allowlisted site that no longer exists fails too — delete its entry.
//!   That is what makes the list shrink-only: it can never quietly hold slack
//!   that a later raw site could spend.
//!
//! The file must also stay sorted, so concurrent slices merge cleanly. When the
//! allowlist is empty the migration is complete.
//!
//! # Why a Rust test and not a `scripts/check-*.sh`
//!
//! Same reason as `gh_api_repo_flag.rs`: the shell-language policy (ADR-0018)
//! admits no new lint script, and a `#[test]` here already runs in CI.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use regex::Regex;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The facade module is the one place allowed to spawn `gh`.
const EXEMPT_DIR: &str = "src/gh_invocation/";

const ALLOWLIST: &str = "tests/fixtures/gh-spawn-allowlist.txt";

fn raw_site_regex() -> Regex {
    Regex::new(
        r#"Command::new\(\s*"gh"\s*\)|Command::new\(\s*[&*]*\s*(?:[A-Za-z_]\w*::)*(?:self\.)?(?:gh|gh_bin\w*)\b|\bfn\s+gh_bin\w*\b"#,
    )
    .expect("valid regex")
}

/// `(1-based line, trimmed text)` for every raw site in `text`.
fn scan(text: &str) -> Vec<(usize, String)> {
    let re = raw_site_regex();
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
        .filter(|(_, line)| re.is_match(line))
        .map(|(i, line)| (i + 1, line.trim().to_string()))
        .collect()
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// One allowlist key per raw site: `src/<path> | <trimmed line>`.
fn key(rel_path: &str, text: &str) -> String {
    format!("{rel_path} | {text}")
}

/// Every raw site in the crate's `src/`, as allowlist keys (with duplicates).
fn tree_sites() -> Vec<String> {
    let root = manifest_dir();
    let mut files = Vec::new();
    collect_rust_files(&root.join("src"), &mut files);
    assert!(
        files.len() > 100,
        "expected to scan the whole crate; only found {} file(s) — the scan's own \
         premise is broken, not the tree",
        files.len()
    );
    let mut sites = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel.starts_with(EXEMPT_DIR) {
            continue;
        }
        let text = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", file.display()));
        sites.extend(scan(&text).into_iter().map(|(_, t)| key(&rel, &t)));
    }
    sites
}

/// Non-blank, non-`#` lines of the allowlist text, in file order.
fn parse_allowlist(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(str::to_string)
        .collect()
}

fn counts(keys: &[String]) -> BTreeMap<&str, usize> {
    let mut m = BTreeMap::new();
    for k in keys {
        *m.entry(k.as_str()).or_insert(0) += 1;
    }
    m
}

/// `(new, stale)`: sites the allowlist does not cover, and allowlist entries
/// no longer present in the tree — each repeated once per surplus occurrence.
fn ratchet_diff(actual: &[String], allowed: &[String]) -> (Vec<String>, Vec<String>) {
    let (a, w) = (counts(actual), counts(allowed));
    let surplus = |x: &BTreeMap<&str, usize>, y: &BTreeMap<&str, usize>| {
        x.iter()
            .flat_map(|(k, n)| {
                let extra = n.saturating_sub(*y.get(k).unwrap_or(&0));
                std::iter::repeat_n((*k).to_string(), extra)
            })
            .collect::<Vec<_>>()
    };
    (surplus(&a, &w), surplus(&w, &a))
}

fn read_allowlist() -> String {
    let path = manifest_dir().join(ALLOWLIST);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

/// The invariant: the tree's raw `gh` spawn sites are exactly the allowlist.
#[test]
fn raw_gh_spawn_sites_match_the_shrink_only_allowlist() {
    let allowed = parse_allowlist(&read_allowlist());
    let (new, stale) = ratchet_diff(&tree_sites(), &allowed);
    let fmt = |v: &[String]| {
        v.iter()
            .map(|s| format!("  {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        new.is_empty() && stale.is_empty(),
        "raw `gh` spawn sites drifted from {ALLOWLIST} (#9985).\n\n\
         NEW raw sites — build these through `crate::gh_invocation::GhInvocation` \
         (or `gh_invocation::gh_bin()` for the resolver) instead; do NOT add them \
         to the allowlist:\n{}\n\n\
         STALE allowlist entries — the site is gone; delete these lines so the \
         ratchet tightens:\n{}",
        fmt(&new),
        fmt(&stale)
    );
}

#[test]
fn the_allowlist_is_sorted() {
    let allowed = parse_allowlist(&read_allowlist());
    let mut sorted = allowed.clone();
    sorted.sort();
    assert_eq!(allowed, sorted, "{ALLOWLIST} must stay byte-sorted (`LC_ALL=C sort`)");
}

/// Negative fixture: the scan's own discriminating power. Without this, a scan
/// that silently stopped matching would report a clean tree forever.
#[test]
fn the_scan_flags_every_raw_site_shape() {
    let raw = r#"
        let a = Command::new("gh");
        let b = Command::new(gh_bin);
        let c = Command::new(&gh_bin);
        let d = Command::new(gh_bin());
        let e = std::process::Command::new(&self.gh_bin);
        let f = tokio::process::Command::new(&gh);
        let g = Command::new( *gh );
        let h = Command::new(crate::gh_invocation::gh_bin());
        let i = tokio::process::Command::new(loom_daemon::forge_cmd::gh_bin());
        fn gh_bin() -> String { String::new() }
        pub fn gh_bin_env() -> String { String::new() }
        fn gh_bin_or_default(&self) -> PathBuf { PathBuf::new() }
"#;
    let hits = scan(raw);
    assert_eq!(hits.len(), 12, "every shape must be flagged: {hits:?}");
}

#[test]
fn the_scan_respects_word_boundaries_and_comments() {
    let clean = r#"
        let a = Command::new("ghost");
        let b = Command::new(&ghost);
        let c = Command::new("git");
        let d = Command::new(&self.gh_path_like);
        let e = Command::new(launcher);
        let f = Command::new(crate::paths::git_bin());
        let g = Command::new(crate::ghost::bin());
        fn ghbin() {}
        // Command::new("gh") in prose is not a site.
        //! nor is `fn gh_bin` in a doc comment.
"#;
    assert!(scan(clean).is_empty(), "{:?}", scan(clean));
}

/// Negative fixture for the ratchet itself: a new site and a stale entry must
/// both be reported, and duplicates are counted, not collapsed.
#[test]
fn the_ratchet_reports_new_sites_and_stale_entries() {
    let s = |v: &[&str]| v.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
    let allowed = s(&["src/a.rs | x", "src/a.rs | x", "src/b.rs | y"]);

    let (new, stale) =
        ratchet_diff(&s(&["src/a.rs | x", "src/a.rs | x", "src/b.rs | y"]), &allowed);
    assert!(new.is_empty() && stale.is_empty());

    let (new, stale) = ratchet_diff(
        &s(&[
            "src/a.rs | x",
            "src/a.rs | x",
            "src/a.rs | x",
            "src/b.rs | y",
        ]),
        &allowed,
    );
    assert_eq!((new, stale), (s(&["src/a.rs | x"]), vec![]), "a third copy is new");

    let (new, stale) = ratchet_diff(&s(&["src/a.rs | x"]), &allowed);
    assert_eq!(new, Vec::<String>::new());
    assert_eq!(stale, s(&["src/a.rs | x", "src/b.rs | y"]), "removed sites must be stale");
}

#[test]
fn allowlist_parsing_skips_comments_and_blanks() {
    let parsed = parse_allowlist("# header\n\nsrc/a.rs | x\n  # indented\nsrc/b.rs | y  \n");
    assert_eq!(parsed, vec!["src/a.rs | x".to_string(), "src/b.rs | y".to_string()]);
}
