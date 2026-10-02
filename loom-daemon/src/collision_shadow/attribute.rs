//! Live outcome attribution for captured shadow pairs (#9920): resolve
//! each captured pair's outcome from the two issues' **merged PRs** on the
//! forge, and emit `OutcomeRecord`s keyed to the capture pair ids.
//!
//! # Semantics (pre-registered with the offline converter's mapping)
//!
//! For a captured pair (A, B):
//!
//! * both sides' merged PRs resolved → their changed-file sets are
//!   compared: shared paths → [`OutcomeKind::TextualConflictPinned`] (the
//!   same file-overlap signal the offline positives carried, with the
//!   merge-tree counterfactual as a follow-up refinement); no overlap →
//!   [`OutcomeKind::Missing`] (a negative in the window);
//! * either side unresolved (no merged PR / still open) →
//!   [`OutcomeKind::Pending`] — never fabricated.
//!
//! Issue→PR resolution uses one `gh pr list --state merged` call total and
//! body-reference matching (`Closes #N` / `Fixes #N` / `Resolves #N`, with
//! digit-boundary care); changed files come from one `gh api …/files` call
//! per PR. Idempotency: records are deterministic over the pair + window,
//! and the CLI skips ids already present in the output file.
//!
//! The merge-tree counterfactual (does B's patch transplant onto A's
//! result?) is a follow-up refinement of the positive signal — the
//! file-overlap semantics here match the offline positives exactly, which
//! is what the §D gates count.

use super::super::collision_evidence::records::{
    Attribution, OutcomeKind, OutcomeRecord, OUTCOME_SCHEMA_VERSION,
};

/// Attribute one pair's outcome from the two issues' merged PR changed
/// files. `None` = that side has no resolved merged PR yet.
pub fn attribute_pair(a_files: Option<Vec<String>>, b_files: Option<Vec<String>>) -> OutcomeKind {
    match (a_files, b_files) {
        (Some(a), Some(b)) => {
            let shared: Vec<String> = a.iter().filter(|p| b.contains(p)).cloned().collect();
            if shared.is_empty() {
                OutcomeKind::Missing {
                    window: "pair integration window".into(),
                }
            } else {
                OutcomeKind::TextualConflictPinned {
                    conflicted_files: shared,
                    replay_provenance: "live merged-PR changed-file overlap \
                                        (forge attribution; merge-tree \
                                        refinement follow-up)"
                        .into(),
                }
            }
        }
        _ => OutcomeKind::Pending,
    }
}

/// Case-insensitive `closes/fixes/resolves #N` reference to `issue` in a
/// PR body — the trailing digit boundary matters ("closes #12" must not
/// match issue #1).
pub fn body_references_issue(body: &str, issue: u32) -> bool {
    let b = body.to_lowercase();
    for verb in ["closes", "fixes", "resolves"] {
        let pat = format!("{verb} #{issue}");
        let mut from = 0;
        while let Some(pos) = b[from..].find(&pat) {
            let abs = from + pos + pat.len();
            let next = b[abs..].chars().next();
            if !next.is_some_and(|c| c.is_ascii_digit()) {
                return true;
            }
            from = abs;
        }
    }
    false
}

/// Match merged PRs to issues from the parsed `gh pr list --json
/// number,body` array: issue → PR number (first matching PR wins).
pub fn map_merged_prs_from_json(
    prs: &[serde_json::Value],
    issues: &[u32],
) -> std::collections::BTreeMap<u32, u32> {
    let mut map = std::collections::BTreeMap::new();
    for issue in issues {
        for pr in prs {
            let (Some(number), Some(body)) = (
                pr.get("number").and_then(|v| v.as_u64()),
                pr.get("body").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            if body_references_issue(body, *issue) {
                map.insert(*issue, number as u32);
                break;
            }
        }
    }
    map
}

/// Changed-file names from the parsed `gh api …/pulls/{n}/files` array.
pub fn filenames_from_files_json(files: &[serde_json::Value]) -> Vec<String> {
    files
        .iter()
        .filter_map(|f| f.get("filename").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
        .collect()
}

/// Attribute every captured pair: resolve both issues' merged PRs (one
/// `gh pr list` call total + one files call per referenced PR), compare
/// changed-file sets, and build one OutcomeRecord per pair — deterministic
/// ids (`collision_evidence::record_id` over the full record) so re-runs
/// are idempotent. `Observation window` = (earliest capture of the pair,
/// now). Counted, non-fatal: a PR whose files call fails records the pair
/// as Pending with the failure noted (the shadow-only contract — a bridge
/// failure never becomes a fabricated verdict).
#[allow(clippy::too_many_arguments)]
pub fn attribute_captures(
    captures: &[super::CaptureRecord],
    gh: &str,
    repo: &str,
    captures_dir: &std::path::Path,
) -> anyhow::Result<Vec<OutcomeRecord>> {
    // Unique pairs, earliest capture wins (a pair may appear in many ticks).
    let mut pairs: std::collections::BTreeMap<String, &super::CaptureRecord> =
        std::collections::BTreeMap::new();
    for c in captures {
        pairs
            .entry(c.unordered_pair_id.clone())
            .and_modify(|existing| {
                if existing.captured_at > c.captured_at {
                    *existing = c;
                }
            })
            .or_insert(c);
    }
    let issues: Vec<u32> = {
        let mut s = std::collections::BTreeSet::new();
        for c in pairs.values() {
            s.insert(c.issue_a);
            s.insert(c.issue_b);
        }
        s.into_iter().collect()
    };

    // ONE `gh pr list --state merged` call for the whole run; body-reference
    // matching locally maps issues to merged PR numbers.
    let pr_list = gh_json(
        gh,
        &[
            "pr",
            "list",
            "--repo",
            repo,
            "--state",
            "merged",
            "--json",
            "number,body",
            "--limit",
            "300",
        ],
    )?;
    let pr_list = pr_list.as_array().cloned().unwrap_or_default();
    let pr_map = map_merged_prs_from_json(&pr_list, &issues);

    let mut out = Vec::new();
    let now = chrono::Utc::now().to_rfc3339();
    for (pair_id, c) in &pairs {
        let a_files = match pr_map.get(&c.issue_a) {
            Some(pr) => gh_changed_files(gh, repo, *pr)?,
            None => None,
        };
        let b_files = match pr_map.get(&c.issue_b) {
            Some(pr) => gh_changed_files(gh, repo, *pr)?,
            None => None,
        };
        let kind = attribute_pair(a_files, b_files);
        let record = OutcomeRecord {
            id: String::new(),
            schema_version: OUTCOME_SCHEMA_VERSION,
            directed_eval_id: format!("{pair_id}:directed-{}-{}", c.issue_a, c.issue_b),
            unordered_pair_id: pair_id.clone(),
            repo: c.repo.clone(),
            kind,
            evidence_refs: vec![],
            observation_window: (c.captured_at.clone(), now.clone()),
            attribution: Attribution {
                event_id: format!("live-attribute-{pair_id}"),
                share: 1.0,
                note: Some(
                    "live forge attribution: merged-PR changed-file overlap                      (merge-tree refinement follow-up)"
                        .into(),
                ),
            },
            recorded_at: now.clone(),
        };
        out.push(record);
    }
    let _ = captures_dir;
    // Deterministic ids over the full record (same contract as #9786).
    for r in out.iter_mut() {
        let id = collision_evidence_id(r)?;
        r.id = id;
    }
    Ok(out)
}

fn gh_json(gh: &str, args: &[&str]) -> anyhow::Result<serde_json::Value> {
    let out = std::process::Command::new(gh)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("gh {args:?}: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "gh {args:?} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .map_err(|e| anyhow::anyhow!("gh {args:?}: response decode: {e}"))
}

fn gh_changed_files(gh: &str, repo: &str, pr: u32) -> anyhow::Result<Option<Vec<String>>> {
    match gh_json(
        gh,
        &[
            "api",
            &format!("repos/{repo}/pulls/{pr}/files"),
            "--paginate",
        ],
    ) {
        Ok(v) => Ok(Some(filenames_from_files_json(
            v.as_array().map(|a| a.as_slice()).unwrap_or(&[]),
        ))),
        // A files call failure (deleted PR, permission) leaves the pair
        // unresolved rather than fabricating an empty diff.
        Err(e) => {
            log::warn!("attribute: pr #{pr} files call failed: {e}");
            Ok(None)
        }
    }
}

fn collision_evidence_id(r: &OutcomeRecord) -> anyhow::Result<String> {
    super::super::collision_evidence::record_id(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64, body: &str) -> serde_json::Value {
        serde_json::json!({ "number": number, "body": body })
    }

    #[test]
    fn shared_changed_paths_are_a_pinned_textual_conflict() {
        let kind = attribute_pair(
            Some(vec!["src/a.rs".into(), "src/shared.rs".into()]),
            Some(vec!["src/b.rs".into(), "src/shared.rs".into()]),
        );
        match kind {
            OutcomeKind::TextualConflictPinned {
                conflicted_files,
                replay_provenance,
            } => {
                assert_eq!(conflicted_files, vec!["src/shared.rs"]);
                assert!(replay_provenance.contains("live"));
            }
            other => panic!("expected TextualConflictPinned, got {other:?}"),
        }
    }

    #[test]
    fn no_overlap_is_a_missing_negative() {
        let kind = attribute_pair(Some(vec!["src/a.rs".into()]), Some(vec!["src/b.rs".into()]));
        match kind {
            OutcomeKind::Missing { window } => {
                assert!(window.contains("integration window"));
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn either_side_unresolved_stays_pending() {
        assert!(matches!(
            attribute_pair(None, Some(vec!["src/a.rs".into()])),
            OutcomeKind::Pending
        ));
        assert!(matches!(attribute_pair(Some(vec![]), None), OutcomeKind::Pending));
    }

    #[test]
    fn body_references_issue_respects_the_digit_boundary() {
        let body = "this PR closes #12 and also fixes #7";
        assert!(body_references_issue(body, 12));
        assert!(body_references_issue(body, 7));
        assert!(!body_references_issue(body, 1));
        assert!(!body_references_issue(body, 2));
        assert!(body_references_issue("Resolves #41 in one go", 41));
    }

    #[test]
    fn merged_pr_matching_maps_issues_to_numbers() {
        let prs = vec![
            pr(100, "unrelated change"),
            pr(101, "Fixes #9906 with a cache fix"),
            pr(102, "closes #9930"),
        ];
        let map = map_merged_prs_from_json(&prs, &[9906, 9930, 9999]);
        assert_eq!(map.get(&9906), Some(&101));
        assert_eq!(map.get(&9930), Some(&102));
        assert_eq!(map.get(&9999), None);
    }

    #[test]
    fn filenames_parse_from_the_files_array() {
        let files = serde_json::json!([
            { "filename": "src/a.rs", "status": "modified" },
            { "filename": "src/b.rs" }
        ]);
        let names = filenames_from_files_json(files.as_array().unwrap());
        assert_eq!(names, vec!["src/a.rs", "src/b.rs"]);
    }
}
