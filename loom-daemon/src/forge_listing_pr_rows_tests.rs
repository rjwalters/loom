//! Tests for pull-request exclusion from issue listings (#9929), extracted to
//! a sibling file so `forge_listing_tests.rs` stays under its file-size ratchet.

use super::*;
use std::path::PathBuf;

/// A REST issues-listing body holding one issue and one pull request — the
/// shape `GET /repos/{owner}/{repo}/issues` actually returns.
const MIXED_LISTING: &str = r#"[
    {"number": 42, "state": "open", "labels": [{"name": "loom:triage"}]},
    {"number": 306, "state": "open", "labels": [],
     "pull_request": {"url": "https://api.github.com/repos/o/r/pulls/306"}}
]"#;

/// Regression for #9929: the issue-side filter must drop every row carrying a
/// `pull_request` key. The incident this guards against is a PR number
/// reaching a Curator run as if it were an issue number (downstream PR #306
/// was labeled `loom:curating` → `loom:curated` + a `tier:` label by Curator
/// automation), which is only possible when a candidate-selection listing
/// serves PR rows to an issue-targeting consumer.
#[test]
fn issues_only_drops_pull_request_rows() {
    let rows = parse_rest_issues(MIXED_LISTING).unwrap();
    assert_eq!(rows.len(), 2, "the raw listing carries both kinds");

    let issues = issues_only(rows.clone());
    assert_eq!(
        issues.iter().map(|r| r.number).collect::<Vec<_>>(),
        vec![42],
        "a PR row must never be offered to an issue-targeting consumer"
    );
    assert!(issues.iter().all(|r| !r.is_pull_request));

    let prs = pull_requests_only(rows);
    assert_eq!(prs.iter().map(|r| r.number).collect::<Vec<_>>(), vec![306]);
    assert!(prs.iter().all(|r| r.is_pull_request));
}

#[test]
fn issues_only_is_identity_on_a_pr_free_listing() {
    let rows = parse_rest_issues(r#"[{"number": 1, "state": "open", "labels": []}]"#).unwrap();
    assert_eq!(issues_only(rows.clone()), rows);
    assert!(pull_requests_only(rows).is_empty());
}

/// The function names that count as "this call site made an explicit decision
/// about pull-request rows".
const PR_DECISION_MARKERS: &[&str] = &[
    "is_pull_request",
    "issues_only",
    "pull_requests_only",
    "list_issues_only_cached",
];

/// The raw listing entry points that hand back UNFILTERED rows (issues *and*
/// pull requests).
const RAW_LISTING_CALLS: &[&str] = &[
    "list_issues_cached(",
    "list_issues_cached_as(",
    "list_issues_cached_persistent(",
    "list_issues_cached_persistent_as(",
];

fn rust_sources_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources_under(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Structural guard for #9929: every module that calls a **raw** REST issues
/// listing must make an explicit decision about pull-request rows.
///
/// `GET /repos/{owner}/{repo}/issues` returns PRs alongside issues, so a call
/// site that never mentions [`RestIssue::is_pull_request`] (nor routes through
/// [`issues_only`] / [`pull_requests_only`] / `list_issues_only_cached_as`) is
/// silently treating PR numbers as issue numbers. That is the defect class
/// behind #9929 — a PR handed to a Curator run as a curation candidate — and
/// auditing it by hand once does not stop the next call site from forgetting.
/// This test is the tie that fails CI instead.
///
/// If this fails for a new call site, the fix is to say what you mean at that
/// call site: use `list_issues_only_cached_as` (issue candidates),
/// `pull_requests_only` (PR queues), or an explicit `is_pull_request` split
/// (a consumer that genuinely wants both).
#[test]
fn every_raw_listing_call_site_decides_about_pull_requests() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources_under(&src, &mut files);
    assert!(files.len() > 50, "source walk found suspiciously few files");

    let mut offenders = Vec::new();
    for path in files {
        let rel = path.strip_prefix(&src).unwrap_or(&path).to_path_buf();
        let rel_str = rel.display().to_string();
        // `forge_listing.rs` DEFINES the raw entry points, and test modules
        // exercise them deliberately.
        if rel_str.starts_with("forge_listing") || rel_str.contains("test") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => panic!("cannot read {}: {e}", path.display()),
        };
        // Only lines that actually CALL a raw listing count — a doc comment
        // naming one (`[\`list_issues_cached\`]`) is prose, not a call.
        let calls_raw = text.lines().any(|line| {
            let code = line.trim_start();
            !code.starts_with("//")
                && !code.starts_with("*")
                && RAW_LISTING_CALLS.iter().any(|needle| code.contains(needle))
        });
        if calls_raw && !PR_DECISION_MARKERS.iter().any(|m| text.contains(m)) {
            offenders.push(rel_str);
        }
    }

    assert!(
        offenders.is_empty(),
        "these modules read the REST issues listing (which returns pull requests too) without \
         ever deciding about `pull_request` rows — see #9929: {offenders:?}"
    );
}

/// Shell call sites that read the REST issues listing and deliberately want
/// **both** issues and pull requests, with the reason. Everything else must
/// filter `pull_request` within a few lines of the request (#9929).
const SHELL_BOTH_KINDS_ALLOWLIST: &[(&str, &str)] = &[(
    "sync-labels.sh",
    "github_label_usage/gitea label-usage probe: answers \"is this label in use \
     anywhere\", for which a PR carrying it counts exactly like an issue",
)];

/// The same structural guard as above, for the shell side of the tree: a
/// `gh api .../issues?…` listing in `defaults/scripts/**.sh` must filter
/// `pull_request` (what `check-duplicate.sh` does) or be allowlisted.
///
/// Shell is where the GraphQL-exhaustion fallbacks live, and a fallback is
/// exactly the path least likely to be exercised before it ships — which is
/// how an unfiltered REST listing stays latent until a rate-limit day turns
/// it into the #9929 defect.
#[test]
fn every_shell_rest_issues_listing_filters_pull_requests() {
    let scripts = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("defaults/scripts");
    if !scripts.is_dir() {
        return; // installed tree without the source `defaults/`
    }
    let mut files = Vec::new();
    rust_sources_under_ext(&scripts, "sh", &mut files);
    assert!(!files.is_empty(), "no shell scripts found under defaults/scripts");

    let mut offenders = Vec::new();
    for path in files {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if path.components().any(|c| c.as_os_str() == "tests")
            || SHELL_BOTH_KINDS_ALLOWLIST.iter().any(|(f, _)| *f == name)
        {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            // A `/issues?` query string is a LISTING read; `/issues/<n>` (a
            // single object) and a bare `/issues` POST (create) are not.
            if !line.contains("/issues?") {
                continue;
            }
            let window = lines[i..lines.len().min(i + 4)].join("\n");
            if !window.contains("pull_request") {
                offenders.push(format!("{name}:{}", i + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these shell REST issues listings do not exclude pull requests (REST /issues returns \
         them too) — filter with `select(.pull_request == null)` or add a reasoned entry to \
         SHELL_BOTH_KINDS_ALLOWLIST; see #9929: {offenders:?}"
    );
}

fn rust_sources_under_ext(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources_under_ext(&path, ext, out);
        } else if path.extension().is_some_and(|e| e == ext) {
            out.push(path);
        }
    }
}
