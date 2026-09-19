//! Tree invariant (Issue #8263): no `gh api` command builder in this crate
//! may append a `--repo` flag.
//!
//! # The bug class
//!
//! `gh api` has **no `--repo` flag**. Only the porcelain subcommands
//! (`gh issue`, `gh pr`, `gh repo`) accept one; `gh api` exits with
//! `unknown flag: --repo` *before issuing any request*, resolving
//! `{owner}`/`{repo}` from the cwd's git remote or from the **`GH_REPO`
//! environment variable** instead.
//!
//! The idiom was copied from the `gh issue`/`gh pr` neighbours into six
//! `gh api` builders. Every one of those probes is deliberately fail-open, so
//! the symptom was never a crash — it was a silent, permanent "cannot verify"
//! on any host exporting `LOOM_REPO`, and a single-owner fleet that never
//! exports it saw nothing at all. That is precisely the shape of defect that
//! survives review by being copied, so the counter-measure is mechanical.
//!
//! # Why this is a Rust test and not a `scripts/check-*.sh`
//!
//! `.loom/docs/shell-language-policy.md` (ADR-0018) closes the category list a
//! NEW `.sh` may claim — `contract` and `settled` are baseline-only — so a new
//! lint script is not admissible. A `#[test]` in `loom-daemon/tests/` is
//! already wired into CI (`cargo nextest run --workspace`, the `backend-tests`
//! job) and needs no new workflow step. The precedent it follows is
//! `defaults_symlink_containment.rs` / `shell_budget_ratchet.rs`: static
//! assertions over the repo tree, no daemon needed.
//!
//! # The fix a finding wants
//!
//! Call `crate::gh_repo_env::apply_loom_repo_override(&mut cmd)` — the single
//! documented place the override is applied, as `GH_REPO`. Do NOT touch the
//! `gh issue` / `gh pr` / `gh repo` call sites: `--repo` is correct there, and
//! the scan below is built to leave them alone.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

/// How far back from a `.arg("--repo")` the scan will look for the builder's
/// subcommand before giving up. Generous enough to span the longest real
/// builder in this crate (subcommand, path, `--paginate`, `--jq`, the filter
/// literal, `current_dir`, the credential-preflight call and their comments),
/// bounded so the scan cannot manufacture a finding from arbitrary distance.
const LOOKBACK_LINES: usize = 60;

/// A `gh api` builder that also appends a `--repo` flag.
#[derive(Debug)]
struct Finding {
    file: String,
    line: usize,
    text: String,
}

/// The receiver a `.arg("--repo")` is being called on — `cmd` in
/// `cmd.arg("--repo").arg(repo);`. `None` when the line does not append a
/// `--repo` argument at all.
fn repo_flag_receiver(line: &str) -> Option<&str> {
    let head = &line[..line.find(r#".arg("--repo")"#)?];
    let ident: String = head
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if ident.is_empty() {
        return None;
    }
    let start = head.len() - ident.len();
    Some(&head[start..])
}

/// The literal a `<receiver>.arg("…")` line passes, when it is a plain string
/// literal on that same receiver. `None` for a chained `.arg(…)` with no
/// receiver, a non-literal argument (`format!(…)`, a variable), or a different
/// receiver.
fn receiver_arg_literal<'a>(line: &'a str, receiver: &str) -> Option<&'a str> {
    let needle = format!("{receiver}.arg(\"");
    let at = line.find(&needle)?;
    // Reject a longer identifier that merely ENDS with `receiver` (`pr` must
    // not match `new_pr.arg("…")`).
    if line[..at]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    let rest = &line[at + needle.len()..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// Scan one Rust source text for the idiom.
///
/// For each `.arg("--repo")`, walk backwards on the SAME receiver to the
/// nearest non-flag string-literal argument — the builder's subcommand — and
/// report a finding only when that subcommand is `api`. Walking the receiver
/// rather than a plain line window is what keeps the scan from attributing a
/// `gh pr list --repo` to a `gh api` builder that happens to sit above it in
/// the same file.
///
/// Deliberately textual and deliberately narrow: this is a re-copy tripwire
/// for a known, named defect, not a proof of absence.
fn scan(text: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut hits = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        // Ignore comments and doc comments — this very file, and the fixed
        // call sites, name the flag in prose on purpose.
        if line.trim_start().starts_with("//") {
            continue;
        }
        let Some(receiver) = repo_flag_receiver(line) else {
            continue;
        };
        let start = i.saturating_sub(LOOKBACK_LINES);
        for j in (start..i).rev() {
            let prev = lines[j];
            if prev.trim_start().starts_with("//") {
                continue;
            }
            let Some(arg) = receiver_arg_literal(prev, receiver) else {
                continue;
            };
            // Flags (`--paginate`, `--jq`, …) are not the subcommand; keep
            // walking back until the subcommand itself is found.
            if arg.starts_with('-') {
                continue;
            }
            if arg == "api" {
                hits.push((i + 1, (*line).trim().to_string()));
            }
            break;
        }
    }
    hits
}

fn crate_src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
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

/// The invariant: zero `gh api` builders append `--repo` anywhere under
/// `loom-daemon/src/`.
#[test]
fn no_gh_api_builder_appends_a_repo_flag() {
    let src = crate_src_dir();
    let mut files = Vec::new();
    collect_rust_files(&src, &mut files);
    assert!(
        files.len() > 100,
        "expected to scan the whole crate; only found {} file(s) under {} — the \
         scan's own premise is broken, not the tree",
        files.len(),
        src.display()
    );

    let mut findings: Vec<Finding> = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", file.display()));
        for (line, text) in scan(&text) {
            findings.push(Finding {
                file: file
                    .strip_prefix(&src)
                    .unwrap_or(file)
                    .display()
                    .to_string(),
                line,
                text,
            });
        }
    }

    assert!(
        findings.is_empty(),
        "`gh api` has NO --repo flag — it exits `unknown flag: --repo` before \
         issuing any request, silently disabling these fail-open probes on every \
         host that exports LOOM_REPO (#8263).\n\
         Call `crate::gh_repo_env::apply_loom_repo_override(&mut cmd)` instead, \
         which applies the override as the GH_REPO env var.\n\n{}",
        findings
            .iter()
            .map(|f| format!("  src/{}:{}: {}", f.file, f.line, f.text))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The gate's own discriminating power, against synthetic fixtures — the
/// `--self-test` role the `scripts/check-*.sh` ratchet family plays for its
/// own gates. Without this, a scan that silently stopped matching anything
/// would keep reporting a clean tree forever.
#[test]
fn the_scan_flags_a_newly_introduced_gh_api_repo_flag() {
    let reintroduced = r#"
        let mut cmd = Command::new(gh_bin);
        cmd.arg("api")
            .arg(format!("repos/{{owner}}/{{repo}}/issues/{issue}"))
            .arg("--jq")
            .arg(".state");
        cmd.current_dir(root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
"#;
    let hits = scan(reintroduced);
    assert_eq!(hits.len(), 1, "the scan must flag a re-introduced `gh api … --repo`: {hits:?}");
    assert!(hits[0].1.contains("--repo"), "{hits:?}");
}

/// The other half of discriminating power: `gh issue` / `gh pr` / `gh repo`
/// builders take `--repo` and MUST NOT be flagged. A scan that fired on them
/// would be turned off within a day.
#[test]
fn the_scan_leaves_gh_issue_and_gh_pr_builders_alone() {
    let porcelain = r#"
        let mut cmd = Command::new(gh_bin);
        cmd.arg("issue")
            .arg("view")
            .arg(issue.to_string())
            .arg("--json")
            .arg("labels");
        cmd.current_dir(root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }

        let mut pr = Command::new(gh_bin);
        pr.arg("pr").arg("list").arg("--state").arg("open");
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            pr.arg("--repo").arg(repo);
        }
"#;
    assert!(
        scan(porcelain).is_empty(),
        "`--repo` is CORRECT on gh issue/pr/repo builders: {:?}",
        scan(porcelain)
    );
}

/// A `--repo` far enough below a `gh api` builder to belong to a different
/// statement is not attributed to it — the lookback window is bounded on
/// purpose, so the scan does not manufacture findings from distance.
#[test]
fn the_scan_does_not_reach_past_its_lookback_window() {
    let mut text = String::from("        cmd.arg(\"api\").arg(\"repos/x\");\n");
    for _ in 0..(LOOKBACK_LINES + 5) {
        text.push_str("        let _ = 1;\n");
    }
    text.push_str("        cmd.arg(\"--repo\").arg(repo);\n");
    assert!(scan(&text).is_empty(), "{:?}", scan(&text));
}

/// The false positive this scan's first draft produced on the real tree: a
/// `gh api` builder in one function, a `gh pr list --repo` builder in the
/// next. Attribution follows the receiver's own nearest subcommand, so only a
/// genuine `gh api … --repo` is reported.
#[test]
fn the_scan_does_not_attribute_a_later_gh_pr_builder_to_an_earlier_gh_api_one() {
    let two_builders = r#"
    fn fetch_comment_bodies(gh_bin: &Path, root: &Path, pr_number: u32) -> Option<Vec<String>> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("api")
            .arg(format!("repos/{{owner}}/{{repo}}/issues/{pr_number}/comments"))
            .arg("--paginate");
        cmd.current_dir(root);
        let out = cmd.output().ok()?;
    }

    fn list_verdict_prs(gh_bin: &Path, root: &Path) -> Result<Vec<VerdictPr>> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("pr")
            .arg("list")
            .arg("--state")
            .arg("open")
            .arg("--json")
            .arg("number,headRefOid,labels");
        cmd.current_dir(root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
    }
"#;
    assert!(scan(two_builders).is_empty(), "{:?}", scan(two_builders));
}

/// Prose that merely mentions the flag — this file, the fixed call sites'
/// comments, the helper's module docs — is never a finding.
#[test]
fn the_scan_ignores_comments_and_doc_comments() {
    let prose = r#"
        cmd.arg("api").arg("repos/x");
        // NEVER write cmd.arg("--repo").arg(repo) here: `gh api` has no such flag.
        //! `gh api` rejects .arg("--repo") outright.
"#;
    assert!(scan(prose).is_empty(), "{:?}", scan(prose));
}
