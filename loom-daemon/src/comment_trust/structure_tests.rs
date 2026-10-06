//! Structural guards for the #9548 High slice: every Rust reader of the
//! markers it covers takes its forge text through [`super::TrustPolicy`].
//!
//! Two checks, both over production code only (inline `#[cfg(test)]`
//! modules and comment lines are ignored):
//!
//! 1. **Files.** Every file whose code names a covered marker is listed in
//!    [`MARKER_FILES`] with what it does with it. A new file fails the test
//!    until someone has checked it and added it.
//! 2. **Call sites.** In every file that fetches comments for a covered
//!    marker, EACH comment fetch (`…/comments`, a `--json comments` field, the
//!    linked-PR closes-graph) sits in a function listed in [`FETCH_SITES`],
//!    and each reader function's body contains its trust call. A second,
//!    unfiltered fetch added to an already-reviewed file fails here (Judge
//!    #9566 non-blocking note 1), which a file-level list cannot catch.

use std::path::Path;

/// Marker names (constants or literal spellings) covered by this slice.
const MARKERS: &[&str] = &[
    "LEASE_MARKER_PREFIX",
    "CLAIM_ACTIVITY_MARKER_PREFIX",
    "STANDDOWN_MARKER_PREFIX",
    "BASE_CONFLICT_MARKER",
    "QUARANTINE_COMMENT_MARKER",
    "ESCALATE_MARKER",
    "CYCLE_MARKER_PREFIX",
    "UNESCALATE_MARKER_PREFIX",
    "ROSTER_MARKER_PREFIX",
    "loom:stale-check-redate",
    "loom:premise-check",
    "closedByPullRequestsReferences",
];

/// Every production file whose code names a covered marker.
const MARKER_FILES: &[(&str, &str)] = &[
    (
        "claim_reconciliation.rs",
        "lease, claim-activity, stand-down readers (FETCH_SITES)",
    ),
    (
        "claim_reconciliation/review_conflict.rs",
        "writer; reader via fetch_comment_bodies",
    ),
    (
        "cli/lease_co_occupancy.rs",
        "lease + lease-yield reader in read_rows (FETCH_SITES, #9631)",
    ),
    ("comment_trust/records.rs", "the H14 filter itself"),
    ("dep_classify/cli.rs", "reads IssueView from dep_classify/forge.rs (trusted)"),
    ("dep_classify/consts.rs", "constants"),
    ("dep_recheck/forge.rs", "a PR's own closing refs, not the linked-PR guard"),
    (
        "forge_check_claim.rs",
        "lease + lease-yield reader in read_freshest_live_lease (FETCH_SITES, #9453)",
    ),
    ("merge_pr/redate.rs", "writer; reader in remedy_with (FETCH_SITES)"),
    (
        "merge_pr/redate/budget.rs",
        "pure parser over remedy_with's trusted listing (#9590)",
    ),
    ("premise_check/cli.rs", "reader in forge_inputs (FETCH_SITES)"),
    ("premise_check/record.rs", "parser over already-filtered chunks"),
    ("quarantine_reconciliation.rs", "reader (FETCH_SITES)"),
    ("role_runner/roster.rs", "writer; reader in read_roster_comments (FETCH_SITES)"),
    ("role_shard/roster.rs", "constants and pure parsers"),
    (
        "stale_blocked/batch.rs",
        "parked issues' own closing refs + comment text for an advisory, not the linked-PR guard",
    ),
    (
        "sweep_registry/guards.rs",
        "writer; reader in read_lease_comments (FETCH_SITES)",
    ),
    ("sweep_registry/quarantine.rs", "writer"),
    ("worktree_ops/gh.rs", "linked-PR probe (FETCH_SITES)"),
];

/// Per reviewed file: every function that fetches comments (or linked PRs),
/// and the trust call its body must contain (`""` for a writer or a fetch
/// that is not a control read).
const FETCH_SITES: &[(&str, &str, &str)] = &[
    (
        "claim_reconciliation.rs",
        "fetch_freshest_lease_updated_at",
        "TrustPolicy::for_root(root).trusted_records(",
    ),
    (
        "claim_reconciliation.rs",
        "fetch_most_recent_claim_activity_at",
        "TrustPolicy::for_root(root).trusted_records(",
    ),
    (
        "claim_reconciliation.rs",
        "fetch_comment_bodies",
        "TrustPolicy::for_root(root).trusted_bodies(",
    ),
    ("sweep_registry/guards.rs", "read_lease_comments", "policy.trusted_ndjson("),
    ("cli/lease_co_occupancy.rs", "read_rows", "policy.trusted_ndjson("),
    ("forge_check_claim.rs", "read_freshest_live_lease", "policy.trusted_ndjson("),
    ("sweep_registry/guards.rs", "issue_body_via_rest", "records::trusted_body("),
    (
        "sweep_registry/guards.rs",
        "probe_open_linked_pr_graphql",
        "parse_open_linked_pr_trusted(",
    ),
    (
        "sweep_registry/guards.rs",
        "probe_open_linked_pr_rest",
        "parse_open_linked_pr_timeline_trusted(",
    ),
    // #10514: leg 0 (the open-PR listing) first; the old union is the fallback.
    ("worktree_ops/gh.rs", "probe_open_linked_pr", "linked_pr_listing::probe("),
    ("worktree_ops/gh.rs", "legacy_union", "parse_open_linked_pr_timeline_trusted("),
    (
        "worktree_ops/linked_pr_listing.rs",
        "probe",
        "classify_open_linked_pr_rows(&rows, issue, owner_repo, &policy)",
    ),
    (
        "worktree_ops/linked_pr_listing.rs",
        "classify_open_linked_pr_rows",
        "policy.known_untrusted(",
    ),
    ("worktree_ops/gh.rs", "parse_open_linked_pr_trusted", "drop_untrusted_fork_prs("),
    // Posting-dedup of its own notice, not a control read.
    ("worktree_ops/gh.rs", "has_recent_orphan_comment", ""),
    // The pure parser behind the `_trusted` wrappers.
    ("worktree_ops/gh.rs", "parse_open_linked_pr", ""),
    (
        "quarantine_reconciliation.rs",
        "trusted_quarantine_comments",
        "TrustPolicy::for_root(root).trusted_records(",
    ),
    ("dep_classify/forge.rs", "read_issue_in", "records::fetch_trusted_comments("),
    // The raw listing goes straight into `trusted_inputs`, whose own body
    // must filter it (`policy.trusted_listing(`), checked below.
    ("premise_check/cli.rs", "forge_inputs", "trusted_inputs("),
    ("premise_check/cli.rs", "trusted_inputs", "policy.trusted_listing("),
    // #10025's GraphQL fallback: it returns the raw listing + issue object to
    // `forge_inputs`, which hands both to `trusted_inputs(` (checked above),
    // so the records reach `policy.trusted_listing(` like the REST ones.
    // `graphql_listing_and_object` selects `comments(...)` through the
    // `COMMENTS_QUERY` const, which the scanner cannot see; listed anyway.
    ("premise_check/cli.rs", "graphql_listing_and_object", ""),
    ("premise_check/cli.rs", "parse_graphql_comments", ""),
    ("merge_pr/redate.rs", "remedy_with", "policy.trusted_listing("),
    ("merge_pr/redate.rs", "post_comment", ""),
    ("role_runner/roster.rs", "read_roster_comments", "trusted_ndjson("),
    ("role_runner/roster.rs", "create_roster_comment", ""),
    ("role_runner/roster.rs", "delete_roster_comment", ""),
    ("role_runner/roster.rs", "patch_roster_comment", ""),
    // `check-stale-blocked` (#10480): a read-only advisory that reports and
    // never acts, reading blocker prose and a parked issue's own closing refs —
    // the same reads `dep_recheck/forge.rs` makes, not a control read. Also
    // `notify-cleared-blockers` (#10515), which acts only by posting an
    // advisory comment and never edits a label.
    ("stale_blocked/batch.rs", "comments", ""),
    ("stale_blocked/batch.rs", "closing_refs_query", ""),
    ("stale_blocked/batch.rs", "parse_closing_refs", ""),
];

/// Production code of a source file: inline test modules cut off, comment
/// lines blanked (kept as empty lines so line numbers still line up).
fn production(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending_cfg_test = false;
    for line in text.lines() {
        let t = line.trim();
        if t == "#[cfg(test)]" {
            pending_cfg_test = true;
            out.push(String::new());
            continue;
        }
        let is_mod = t.starts_with("mod ") || t.starts_with("pub mod ");
        if pending_cfg_test && is_mod && !t.ends_with(';') {
            break;
        }
        // Stay pending across further (possibly multi-line) attributes; any
        // other item ends it.
        if is_mod || line.starts_with(|c: char| c.is_ascii_alphabetic()) {
            pending_cfg_test = false;
        }
        out.push(if t.starts_with("//") {
            String::new()
        } else {
            line.to_string()
        });
    }
    out
}

/// The name of the function a line belongs to (the last `fn` opened above it).
fn fn_name(line: &str) -> Option<String> {
    let t = line.trim_start();
    let rest = ["pub(crate) fn ", "pub(super) fn ", "pub fn ", "fn "]
        .iter()
        .find_map(|p| t.strip_prefix(p))?;
    Some(
        rest.chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect(),
    )
}

/// `(function, body)` pairs of a file's production code.
fn functions(lines: &[String]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some(name) = fn_name(line) {
            out.push((name, String::new()));
        } else if line.starts_with(|c: char| c.is_ascii_alphabetic()) {
            // A new top-level item (`const`, `struct`, `impl`, …): not a fn.
            out.push(("<item>".to_string(), String::new()));
        }
        if let Some((_, body)) = out.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    out
}

fn is_fetch(line: &str) -> bool {
    line.contains("/comments")
        || line.contains("\"comments\"")
        || line.contains("closedByPullRequestsReferences")
}

fn src() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn is_test_file(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.contains("tests") || name == "test_support.rs" || rel.contains("/tests/")
}

#[test]
fn every_covered_marker_file_is_reviewed() {
    let root = src();
    let mut offenders = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let rel = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if is_test_file(&rel) || rel == "comment_trust/structure_tests.rs" {
                continue;
            }
            let code = production(&std::fs::read_to_string(&path).unwrap_or_default()).join("\n");
            let hit = MARKERS.iter().any(|m| code.contains(m));
            if hit && !MARKER_FILES.iter().any(|(f, _)| *f == rel) {
                offenders.push(rel);
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these files handle a #9548-covered marker but are not reviewed: read their comments \
         through comment_trust::TrustPolicy and list them in MARKER_FILES / FETCH_SITES: \
         {offenders:?}"
    );
}

#[test]
fn every_comment_fetch_in_a_reviewed_file_is_a_filtered_call_site() {
    let root = src();
    let files: std::collections::BTreeSet<&str> = FETCH_SITES.iter().map(|(f, _, _)| *f).collect();
    let mut problems = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        let fns = functions(&production(&text));
        for (name, body) in &fns {
            if name.starts_with('<') || !body.lines().any(is_fetch) {
                continue;
            }
            match FETCH_SITES.iter().find(|(f, n, _)| *f == file && n == name) {
                None => problems.push(format!("{file}: `{name}` fetches comments unreviewed")),
                Some((_, _, need)) if !need.is_empty() && !body.contains(need) => {
                    problems.push(format!("{file}: `{name}` no longer calls `{need}`"));
                }
                Some(_) => {}
            }
        }
        for (f, name, need) in FETCH_SITES.iter().filter(|(f, _, _)| *f == file) {
            match fns.iter().find(|(n, _)| n == name) {
                None => problems.push(format!("{f}: reviewed function `{name}` no longer exists")),
                // A listed reader keeps its trust call even when its fetch
                // moved behind a helper (e.g. `forge_inputs`, whose listing
                // comes from `records::fetch_comment_listing`).
                Some((_, body)) if !need.is_empty() && !body.contains(need) => {
                    problems.push(format!("{f}: `{name}` no longer calls `{need}`"));
                }
                Some(_) => {}
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// The review-conflict pass has no fetch of its own: its "is this flag
/// ours?" read is `fetch_comment_bodies`, which is filtered (H9).
#[test]
fn the_base_conflict_reader_uses_the_filtered_fetch() {
    let text =
        std::fs::read_to_string(src().join("claim_reconciliation/review_conflict.rs")).unwrap();
    let code = production(&text).join("\n");
    assert!(code.contains("forge::fetch_comment_bodies(gh_bin, root, pr.number)"));
    assert!(!code.lines().any(is_fetch), "a second, unfiltered comment fetch appeared");
}
