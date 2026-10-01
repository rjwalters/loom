//! End-to-end contract for `loom-daemon overlap-replay` (#9785).
//!
//! Builds real fixture git repositories and drives the `validate` /
//! `outcomes` / `score` verbs through the real binary, covering the issue's
//! test matrix:
//!
//! - historical issue edits and outcome leakage: unreconstructable snapshots
//!   are excluded from leakage-controlled scoring and counted; a prediction
//!   whose content hash does not match its snapshot is never scored
//! - independent replay of frozen inputs: `score --outcomes-dir` runs with no
//!   git repo at all, and repeated scoring of frozen inputs is byte-stable
//! - duplicate snippets / unequal result sizes: interval dedup + unequal
//!   precision/recall on the fixture predictions
//! - revision/rename mapping, insertion/new-file cases: rename + new-file
//!   branches in the fixture repo
//! - base-update contamination: a rebased branch is flagged; `own_commits`
//!   restricts the patch to the PR's own commits
//! - conflict-resolved final PRs: `final_head_sha` pins the post-repair head
//!   as a separate endpoint without replacing the original-conflict head
//! - ambiguous issue/PR links: `ambiguous` association is evaluated
//!   separately from the primary cohort
//! - missing historical snapshots: an explicitly-missing prediction is a
//!   recorded stratum, not a failure

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// fixture git repo
// ---------------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit_file(dir: &Path, branch: Option<&str>, path: &str, content: &str, msg: &str) -> String {
    if let Some(b) = branch {
        git(dir, &["checkout", "-q", b]);
    }
    let abs = dir.join(path);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&abs, content).unwrap();
    git(dir, &["add", path]);
    git(dir, &["commit", "-q", "-m", msg]);
    git(dir, &["rev-parse", "HEAD"])
}

struct Fixture {
    _dir: TempDir,
    repo: PathBuf,
    base: String,
    feat_a: String,
    feat_b: String,
    feat_c: String,
    feat_d: String,
    feat_e_pre_rebase: String,
    feat_e_rebased: String,
    main_after_u1: String,
    feat_f_renamed: String,
}

/// One repo, many branches, all pinned. Branch A and B both edit line 5 of
/// `src/shared.rs` differently (guaranteed textual conflict) and each add
/// their own new file; C and D are disjoint; E is rebased onto an advanced
/// main (contamination); F renames `src/util.rs`.
fn setup_repo() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo_path = dir.path().to_path_buf();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    let shared: String = (1..=10).map(|i| format!("line{i} = {i}\n")).collect();
    let _base_first = commit_file(dir.path(), None, "src/shared.rs", &shared, "base");
    let _ = commit_file(
        dir.path(),
        None,
        "src/util.rs",
        "// util module v1\nfn util_old() {}\nfn helper_a() {}\nfn helper_b() {}\n// end util\n",
        "base util",
    );
    // The fork point is the tip holding BOTH base files — PRs pin this.
    let base = git(dir.path(), &["rev-parse", "HEAD"]);
    git(dir.path(), &["branch", "-M", "main"]);
    // Named branch at the fork point so fixture branches can `checkout -b x base`.
    git(dir.path(), &["branch", "-q", "base", &base]);

    // feat-a: edit shared line 5, add a new file.
    let a = shared.replace("line5 = 5\n", "line5 = fifty (from A)\n");
    git(dir.path(), &["checkout", "-q", "-b", "feat-a", "main"]);
    let _a_edit = commit_file(dir.path(), None, "src/shared.rs", &a, "A edits shared");
    let feat_a = commit_file(dir.path(), None, "src/a_new.rs", "fn a() {}\n", "A adds a_new");

    // feat-b: edit shared line 5 differently, add b_new.
    git(dir.path(), &["checkout", "-q", "-b", "feat-b", "base"]);
    let b = shared.replace("line5 = 5\n", "line5 = fifty-five (from B)\n");
    let _b_edit = commit_file(dir.path(), None, "src/shared.rs", &b, "B edits shared");
    let feat_b = commit_file(dir.path(), None, "src/b_new.rs", "fn b() {}\n", "B adds b_new");

    // feat-c: edit util only. feat-d: add d_new only.
    git(dir.path(), &["checkout", "-q", "-b", "feat-c", "base"]);
    let feat_c = commit_file(dir.path(), None, "src/util.rs", "fn util_new() {}\n", "C edits util");
    git(dir.path(), &["checkout", "-q", "-b", "feat-d", "base"]);
    let feat_d = commit_file(dir.path(), None, "src/d_new.rs", "fn d() {}\n", "D adds d_new");

    // feat-e: edit util on a branch, then advance main and rebase onto it.
    git(dir.path(), &["checkout", "-q", "-b", "feat-e", "base"]);
    let feat_e_pre_rebase =
        commit_file(dir.path(), None, "src/e_only.rs", "fn e() {}\n", "E edits e_only");
    git(dir.path(), &["checkout", "-q", "main"]);
    let main_after_u1 =
        commit_file(dir.path(), None, "src/upstream.rs", "fn upstream() {}\n", "upstream commit");
    git(dir.path(), &["checkout", "-q", "feat-e"]);
    git(dir.path(), &["rebase", "-q", "main"]);
    let feat_e_rebased = git(dir.path(), &["rev-parse", "HEAD"]);

    // feat-f: rename util.rs → util2.rs with a small edit (content is similar
    // enough for git's rename detection to pair the delete + add).
    git(dir.path(), &["checkout", "-q", "-b", "feat-f", "base"]);
    let _f_rename_commit = commit_file(
        dir.path(),
        None,
        "src/util2.rs",
        "// util module v1\nfn util_renamed() {}\nfn helper_a() {}\nfn helper_b() {}\n// end util\n",
        "F renames util",
    );
    git(dir.path(), &["rm", "-q", "src/util.rs"]);
    git(dir.path(), &["commit", "-q", "-m", "F deletes util"]);
    let feat_f_renamed = git(dir.path(), &["rev-parse", "HEAD"]);

    git(dir.path(), &["checkout", "-q", "main"]);
    Fixture {
        _dir: dir,
        repo: repo_path,
        base,
        feat_a,
        feat_b,
        feat_c,
        feat_d,
        feat_e_pre_rebase,
        feat_e_rebased,
        main_after_u1,
        feat_f_renamed,
    }
}

// ---------------------------------------------------------------------------
// manifest + prediction builders
// ---------------------------------------------------------------------------

fn snapshot_json(issue: u32, reconstructed: bool, affected: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "issue": issue,
        "title": format!("issue {issue} title"),
        "body": format!("issue {issue} body"),
        "curator_affected_files": affected,
        "affected_files_known": !affected.is_empty(),
        "provenance": {
            "method": "timeline replay",
            "edited_after_cutoff": false,
            "reconstructed": reconstructed,
            "exclusion_reason": if reconstructed { None } else { Some("earlier content unavailable") },
        },
        "created_at": "2026-01-01T00:00:00Z",
    })
}

fn pr_json(
    pr: u32,
    issue: u32,
    base: &str,
    head: &str,
    own: Option<Vec<String>>,
) -> serde_json::Value {
    serde_json::json!({
        "pr": pr,
        "issue": issue,
        "base_sha": base,
        "head_sha": head,
        "own_commits": own,
        "provenance": "closing link",
        "observed_conflict": false,
    })
}

fn pair_json(
    id: &str,
    historical: &str,
    issues: [serde_json::Value; 2],
    association: &str,
    prs: Vec<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "pair_id": id,
        "historical_commit": historical,
        "cutoff": "2026-01-02T00:00:00Z",
        "issues": issues,
        "association": association,
        "prs": prs,
        "selection": "same-week same-label",
    })
}

fn manifest_json(pairs: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({
        "manifest_version": 1,
        "repo": "o/r",
        "created_at": "2026-10-01T00:00:00Z",
        "query_policy_version": "qp-v1",
        "pairs": pairs,
    })
}

/// Hash helper mirroring `ReplayManifest::snapshot_content_hash` through the
/// public API, so the fixture predictions pin the right content.
fn content_hash(title: &str, body: &str) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(title.as_bytes());
    h.update(b"\n");
    h.update(body.as_bytes());
    hex::encode(h.finalize())
}

fn prediction_json(
    issue: u32,
    hash: &str,
    source: &str,
    files: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "artifact_version": 1,
        "issue": issue,
        "issue_content_hash": hash,
        "source_revision": source,
        "query_policy_version": "qp-v1",
        "provenance": { "provider": "augment", "model": "m", "index_version": "idx-1" },
        "retrieved_at": "2026-01-02T00:00:00Z",
        "status": { "kind": "present", "files": files },
    })
}

fn write_json(path: &Path, v: &serde_json::Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

// ---------------------------------------------------------------------------
// the suite
// ---------------------------------------------------------------------------

fn daemon() -> Command {
    Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
}

fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    String::from_utf8(std::fs::read(path).unwrap())
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn overlap_replay_end_to_end() {
    let fx = setup_repo();
    let work = tempfile::tempdir().unwrap();
    let manifest_path = work.path().join("manifest.json");
    let preds_dir = work.path().join("preds");
    let out_dir = work.path().join("report");

    // p1: A vs B — same-file conflict pair, common-source line comparison.
    // p2: C vs D — disjoint pair, clean merge.
    // p3: one side's snapshot unreconstructable → excluded from scoring.
    // p4: ambiguous association, explicitly-missing prediction (recorded).
    let manifest = manifest_json(vec![
        pair_json(
            "p1-conflict",
            &fx.base,
            [
                snapshot_json(1, true, &["src/shared.rs"]),
                snapshot_json(2, true, &["src/shared.rs"]),
            ],
            "independent",
            vec![
                pr_json(11, 1, &fx.base, &fx.feat_a, None),
                pr_json(12, 2, &fx.base, &fx.feat_b, None),
            ],
        ),
        pair_json(
            "p2-disjoint",
            &fx.base,
            [
                snapshot_json(3, true, &["src/util.rs"]),
                snapshot_json(4, true, &["src/other.rs"]),
            ],
            "independent",
            vec![
                pr_json(13, 3, &fx.base, &fx.feat_c, None),
                pr_json(14, 4, &fx.base, &fx.feat_d, None),
            ],
        ),
        pair_json(
            "p3-excluded",
            &fx.base,
            [
                snapshot_json(5, false, &[]),
                snapshot_json(6, true, &["src/util.rs"]),
            ],
            "independent",
            vec![
                pr_json(15, 5, &fx.base, &fx.feat_c, None),
                pr_json(16, 6, &fx.base, &fx.feat_d, None),
            ],
        ),
        pair_json(
            "p4-ambiguous",
            &fx.base,
            [snapshot_json(7, true, &[]), snapshot_json(8, true, &[])],
            "ambiguous",
            vec![
                pr_json(17, 7, &fx.base, &fx.feat_e_rebased, None),
                pr_json(18, 8, &fx.base, &fx.feat_e_rebased, None),
            ],
        ),
    ]);
    write_json(&manifest_path, &manifest);

    // Predictions: issues 1 and 2 present (correct pins); issue 3's
    // prediction has a WRONG content hash (mismatch — never scored);
    // issue 7's is explicitly missing.
    let p1_files = serde_json::json!([
        { "path": "src/shared.rs", "intervals": [{"start": 5, "end": 5}],
          "symbols": ["shared_fn"], "intent": "edit" },
        { "path": "src/only_a.txt", "intervals": [], "intent": "context" },
    ]);
    let p2_files = serde_json::json!([
        { "path": "src/shared.rs", "intervals": [{"start": 1, "end": 3}],
          "symbols": [], "intent": "edit" },
    ]);
    write_json(
        &preds_dir.join("1.json"),
        &prediction_json(1, &content_hash("issue 1 title", "issue 1 body"), &fx.base, p1_files),
    );
    write_json(
        &preds_dir.join("2.json"),
        &prediction_json(
            2,
            &content_hash("issue 2 title", "issue 2 body"),
            &fx.base,
            p2_files.clone(),
        ),
    );
    write_json(&preds_dir.join("3.json"), &prediction_json(3, "wrong-hash", &fx.base, p2_files));
    write_json(
        &preds_dir.join("7.json"),
        &serde_json::json!({
            "artifact_version": 1,
            "issue": 7,
            "issue_content_hash": &content_hash("issue 7 title", "issue 7 body"),
            "source_revision": &fx.base,
            "query_policy_version": "qp-v1",
            "provenance": { "provider": "augment", "model": "m", "index_version": "idx-1" },
            "retrieved_at": "2026-01-02T00:00:00Z",
            "status": { "kind": "missing", "reason": "no cache entry" },
        }),
    );

    // --- validate ---------------------------------------------------------
    let out = daemon()
        .args(["overlap-replay", "validate"])
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--predictions-dir")
        .arg(&preds_dir)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "validate failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("content-hash mismatch"),
        "validate must flag the wrong-hash prediction: {stdout}"
    );
    assert!(stdout.contains("explicitly missing"), "{stdout}");

    // --- outcomes ---------------------------------------------------------
    let outcomes_dir = work.path().join("outcomes");
    let out = daemon()
        .args(["overlap-replay", "outcomes"])
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--repo")
        .arg(&fx.repo)
        .arg("--out-dir")
        .arg(&outcomes_dir)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "outcomes failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // --- score (offline: outcomes dir, no --repo) --------------------------
    let run_score = |out_dir: &Path| -> String {
        let out = daemon()
            .args(["overlap-replay", "score"])
            .arg("--manifest")
            .arg(&manifest_path)
            .arg("--outcomes-dir")
            .arg(&outcomes_dir)
            .arg("--predictions-dir")
            .arg(&preds_dir)
            .arg("--out-dir")
            .arg(out_dir)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "score failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    run_score(&out_dir);

    // Determinism: a second score of the same frozen inputs is byte-stable.
    let out_dir2 = work.path().join("report2");
    run_score(&out_dir2);
    for f in [
        "per_pair.jsonl",
        "per_issue.jsonl",
        "per_pair.csv",
        "summary.md",
    ] {
        let a = std::fs::read(out_dir.join(f)).unwrap();
        let b = std::fs::read(out_dir2.join(f)).unwrap();
        assert_eq!(a, b, "{f} must replay byte-identically from frozen inputs");
    }

    // --- per-pair assertions ----------------------------------------------
    let per_pair = read_jsonl(&out_dir.join("per_pair.jsonl"));
    let by_id: BTreeMap<String, serde_json::Value> = per_pair
        .iter()
        .map(|p| (p["pair_id"].as_str().unwrap().to_string(), p.clone()))
        .collect();

    // p1: shared changed file + textual conflict in both replay orders +
    // exact common-source line overlap.
    let p1 = &by_id["p1-conflict"];
    assert_eq!(
        p1["actual"]["shared_changed_files"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "p1 record: {p1}"
    );
    assert_eq!(p1["actual"]["coordinate_basis"], "common_source");
    let line_frac = p1["actual"]["line_overlap_fraction"].as_f64().unwrap();
    assert!(line_frac > 0.0, "both branches edit line 5: overlap must be > 0");
    assert_eq!(p1["conflict"]["any_conflict"], serde_json::json!(true));
    assert_eq!(p1["conflict"]["conflicted_files"], serde_json::json!(["src/shared.rs"]));
    assert_eq!(p1["conflict"]["provenance"], "counterfactual");
    // Leakage-controlled, with both predictions scored.
    assert_eq!(p1["leakage_controlled"], serde_json::json!(true));

    // p2: disjoint — no shared file, clean merge, still common-source.
    let p2 = &by_id["p2-disjoint"];
    assert_eq!(
        p2["actual"]["shared_changed_files"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(p2["conflict"]["any_conflict"], serde_json::json!(false));
    assert_eq!(p2["actual"]["coordinate_basis"], "common_source");

    // p4: ambiguous association recorded as such, evaluated separately.
    assert_eq!(p4_assoc(&by_id), "ambiguous");

    // B1 regression: p3 has NO usable predictions (issue 5's snapshot is
    // excluded, issue 6 has no artifact) — its predicted-overlap record is
    // null and every retrieval-based score is null, never a fabricated zero.
    // The Curator baseline stays scoreable; p1 (both present) scores fully.
    let p3 = &by_id["p3-excluded"];
    assert!(p3["predicted"].is_null(), "p3: {}", p3["predicted"]);
    assert!(p3["scores"]["blend"].is_null());
    assert!(p3["scores"]["file_jaccard"].is_null());
    assert!(p1["predicted"].is_object());
    assert!(p1["scores"]["blend"].as_f64().is_some());

    // --- per-issue assertions ---------------------------------------------
    let per_issue = read_jsonl(&out_dir.join("per_issue.jsonl"));
    let by_issue: BTreeMap<u32, serde_json::Value> = per_issue
        .iter()
        .map(|e| (e["issue"].as_u64().unwrap() as u32, e.clone()))
        .collect();
    // Issue 1: prediction pinned to the snapshot content → scored; the
    // predicted line-5 interval matches the actual line-5 edit exactly.
    let i1 = &by_issue[&1];
    assert_eq!(i1["prediction_status"], "present");
    assert_eq!(i1["file_recall"], serde_json::json!(0.5)); // 1 of 2 changed files predicted
    assert!(
        i1["interval_recall"].as_f64().unwrap_or(-1.0) > 0.0,
        "issue 1 record: {i1}\noutcomes: {}",
        std::fs::read_to_string(outcomes_dir.join("p1-conflict.outcomes.json")).unwrap()
    );
    assert!(!i1["missed_files"].as_array().unwrap().is_empty());
    // Issue 3: wrong-hash prediction is a mismatch — never scored.
    assert_eq!(by_issue[&3]["prediction_status"], "mismatch:content-hash");
    // Issue 5: unreconstructable snapshot → excluded.
    assert!(by_issue[&5]["prediction_status"]
        .as_str()
        .unwrap()
        .starts_with("excluded:"));
    // Issue 7: explicit missing is recorded, not an error.
    assert_eq!(by_issue[&7]["prediction_status"], "missing:no cache entry");

    // --- summary.md contents ----------------------------------------------
    let summary = String::from_utf8(std::fs::read(out_dir.join("summary.md")).unwrap()).unwrap();
    assert!(summary.contains("1 issue snapshot occurrence(s) could not be reconstructed"));
    assert!(summary.contains("1 issue(s) had a prediction whose content hash"));
    assert!(summary.contains("unknown")); // the ambiguous pair's conflict row stratum
    assert!(summary.contains("not a probability")); // ranking-not-probability language

    // --- base-update contamination + own_commits fallback ------------------
    // Without own_commits, the rebased branch absorbs the upstream commit.
    use loom_daemon::overlap_replay::patch::{own_patch, CoordinateBasis};
    let contaminated = own_patch(&fx.repo, 999, &fx.base, &fx.feat_e_rebased, None).unwrap();
    assert!(contaminated.base_update_contamination);
    let files: Vec<&str> = contaminated.files.iter().map(|f| f.path.as_str()).collect();
    assert!(
        files.contains(&"src/upstream.rs"),
        "inherited changes appear in the range patch: {files:?}"
    );
    // With own_commits, the patch counts only the PR's own commit.
    let own = own_patch(
        &fx.repo,
        999,
        &fx.base,
        &fx.feat_e_rebased,
        Some(std::slice::from_ref(&fx.feat_e_rebased)),
    )
    .unwrap();
    let own_files: Vec<&str> = own.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(own_files, vec!["src/e_only.rs"]);
    // Contamination is now DETECTED in commit mode too (the branch does
    // contain upstream commits) — the flag documents the detection while the
    // patch still counts only the PR's own commit.
    assert!(own.base_update_contamination);
    assert_eq!(own.coordinate_basis, CoordinateBasis::PerCommitParent);
    // The pre-rebase commit is a distinct endpoint that still exists.
    assert_ne!(fx.feat_e_pre_rebase, fx.feat_e_rebased);
    assert_ne!(fx.main_after_u1, fx.base);

    // --- rename mapping -----------------------------------------------------
    let renamed = own_patch(&fx.repo, 998, &fx.base, &fx.feat_f_renamed, None).unwrap();
    let util2 = renamed
        .files
        .iter()
        .find(|f| f.path == "src/util2.rs")
        .expect("rename target recorded");
    assert_eq!(util2.path_before, "src/util.rs");

    // --- conflict-resolved final PR endpoint stays separate ------------------
    // A manifest that pins final_head_sha keeps both endpoints; the outcomes
    // verb scores the original head, and the final head resolves the
    // conflict (repair conceals it).
    let repaired = manifest_json(vec![pair_json(
        "p-final",
        &fx.base,
        [
            snapshot_json(21, true, &["src/shared.rs"]),
            snapshot_json(22, true, &["src/shared.rs"]),
        ],
        "independent",
        vec![
            {
                let mut pr = pr_json(31, 21, &fx.base, &fx.feat_a, None);
                pr["final_head_sha"] = serde_json::json!(fx.feat_a);
                pr
            },
            {
                let mut pr = pr_json(32, 22, &fx.base, &fx.feat_b, None);
                // Final head = B's changes merged with A's resolution: the
                // conflict is resolved there, but the ORIGINAL head stays
                // the conflict endpoint.
                pr["final_head_sha"] = serde_json::json!(git(&fx.repo, &["rev-parse", "feat-b",]));
                pr
            },
        ],
    )]);
    let mpath = work.path().join("manifest-final.json");
    write_json(&mpath, &repaired);
    let odir = work.path().join("outcomes-final");
    let out = daemon()
        .args(["overlap-replay", "outcomes"])
        .arg("--manifest")
        .arg(&mpath)
        .arg("--repo")
        .arg(&fx.repo)
        .arg("--out-dir")
        .arg(&odir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let rec: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(odir.join("p-final.outcomes.json")).unwrap())
            .unwrap();
    // The replay still uses the ORIGINAL heads (conflict detected).
    assert_eq!(rec["conflict_replays"].as_array().unwrap().len(), 2);
    assert_eq!(rec["overlap"]["coordinate_basis"], "common_source");
}

fn p4_assoc(by_id: &BTreeMap<String, serde_json::Value>) -> String {
    by_id["p4-ambiguous"]["association"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn score_requires_outcomes_coverage() {
    let fx = setup_repo();
    let work = tempfile::tempdir().unwrap();
    let manifest_path = work.path().join("m.json");
    write_json(
        &manifest_path,
        &manifest_json(vec![pair_json(
            "p1",
            &fx.base,
            [
                snapshot_json(1, true, &["src/shared.rs"]),
                snapshot_json(2, true, &[]),
            ],
            "independent",
            vec![
                pr_json(11, 1, &fx.base, &fx.feat_a, None),
                pr_json(12, 2, &fx.base, &fx.feat_b, None),
            ],
        )]),
    );
    // Empty outcomes dir → score must refuse (missing pair), not silently
    // emit an empty report.
    let empty = work.path().join("outcomes-empty");
    std::fs::create_dir_all(&empty).unwrap();
    let out = daemon()
        .args(["overlap-replay", "score"])
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--outcomes-dir")
        .arg(&empty)
        .arg("--out-dir")
        .arg(work.path().join("r"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("missing pair"));
}

#[test]
fn baseline_only_score_runs_without_predictions() {
    // No --predictions-dir: every prediction is a recorded missing stratum,
    // the Curator baseline still scores, and the report says so.
    let fx = setup_repo();
    let work = tempfile::tempdir().unwrap();
    let manifest_path = work.path().join("m.json");
    write_json(
        &manifest_path,
        &manifest_json(vec![pair_json(
            "p1",
            &fx.base,
            [
                snapshot_json(1, true, &["src/shared.rs"]),
                snapshot_json(2, true, &["src/shared.rs"]),
            ],
            "independent",
            vec![
                pr_json(11, 1, &fx.base, &fx.feat_a, None),
                pr_json(12, 2, &fx.base, &fx.feat_b, None),
            ],
        )]),
    );
    let out_dir = work.path().join("r");
    let out = daemon()
        .args(["overlap-replay", "score"])
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--repo")
        .arg(&fx.repo)
        .arg("--out-dir")
        .arg(&out_dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let summary = String::from_utf8(std::fs::read(out_dir.join("summary.md")).unwrap()).unwrap();
    assert!(summary.contains("2 issue(s) had no frozen prediction"));
    let per_issue = read_jsonl(&out_dir.join("per_issue.jsonl"));
    assert!(per_issue
        .iter()
        .all(|e| e["prediction_status"] == "missing"));
    // Curator baseline still computed (both sides known); every
    // retrieval-based score is null — an unknown, never a fabricated zero.
    let per_pair = read_jsonl(&out_dir.join("per_pair.jsonl"));
    let curator = per_pair[0]["scores"]["curator_baseline"]
        .as_f64()
        .expect("curator baseline scored");
    assert!((curator - 1.0).abs() < 1e-9); // identical affected-files lists
    assert!(per_pair[0]["scores"]["blend"].is_null());
    assert!(per_pair[0]["scores"]["file_jaccard"].is_null());
    assert!(per_pair[0]["predicted"].is_null());
}
