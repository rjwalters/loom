//! Fixtures for #10077: releasing recorded holds between PRs that share no
//! changed file, the shared per-tick files cache, and a seeded property test
//! over the direct-overlap planner.

use std::cell::Cell;

use super::super::{
    plan_repo, plan_repo_with, release_comment_body, EdgeReason, SequenceGroup, SEQUENCE_LABEL,
};
use super::*;
use crate::merge_pr::sequence::marker_text;

fn sha(n: u32) -> String {
    format!("{n:040x}")
}

fn pr(number: u32, created: &str, labels: &[&str]) -> SequencePr {
    SequencePr {
        number,
        created_at: created.to_string(),
        updated_at: created.to_string(),
        head_sha: Some(sha(number)),
        head_ref: format!("feature/issue-{number}"),
        base_ref: "main".to_string(),
        draft: false,
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
    }
}

fn set(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|p| (*p).to_string()).collect()
}

/// The marker `follower` carries, pinned at both current heads.
fn marker(follower: u32, after: u32, source: Option<&str>, plan: &str) -> SequenceMarker {
    SequenceMarker {
        after,
        pred_head: sha(after),
        follower_head: sha(follower),
        plan: plan.into(),
        source: source.map(str::to_string),
    }
}

fn soft(follower: u32, after: u32) -> SequenceMarker {
    marker(follower, after, Some("pass"), "seq-0badc0de")
}

/// The predecessor as the pulls API reports it: open at `head`.
fn pred_at(head: String) -> PredecessorState {
    PredecessorState {
        open: true,
        merged: false,
        head_sha: Some(head),
        updated_at: None,
    }
}

/// The #10056-after-#10053 shape: two PRs, one recorded soft edge between
/// them, file sets supplied per case.
struct Case {
    pred: SequencePr,
    follower: SequencePr,
    open: Vec<SequencePr>,
}

fn case() -> Case {
    let pred = pr(9, "2026-10-01T00:00:00Z", &[]);
    let follower = pr(10, "2026-10-02T00:00:00Z", &[SEQUENCE_LABEL]);
    let open = vec![
        pred.clone(),
        follower.clone(),
        pr(11, "2026-10-02T01:00:00Z", &[]),
    ];
    Case {
        pred,
        follower,
        open,
    }
}

/// Run [`with_no_overlap`] with a fetch that answers from `sets` and counts
/// its calls.
fn decide(
    action: HoldAction,
    m: &SequenceMarker,
    pred_state: Option<&PredecessorState>,
    c: &Case,
    sets: &BTreeMap<u32, BTreeSet<String>>,
) -> (HoldAction, usize) {
    let calls = Cell::new(0);
    let mut cache = TickFiles::default();
    let fetch = |p: &SequencePr| {
        calls.set(calls.get() + 1);
        sets.get(&p.number).cloned()
    };
    let out = with_no_overlap(action, m, pred_state, &c.follower, &c.open, &mut cache, fetch);
    (out, calls.get())
}

fn disjoint() -> BTreeMap<u32, BTreeSet<String>> {
    BTreeMap::from([(9, set(&["docs/a.md"])), (10, set(&["src/b.rs"]))])
}

// --- Release -------------------------------------------------------------

#[test]
fn a_soft_in_flight_hold_between_disjoint_prs_is_released() {
    let c = case();
    let m = soft(10, 9);
    let p = pred_at(sha(9));
    let f = disjoint();
    assert!(no_overlap_release(&m, &c.follower, Some(&c.pred), f.get(&10), f.get(&9)));
    assert_eq!(
        decide(HoldAction::HoldSoft, &m, Some(&p), &c, &f),
        (HoldAction::ReleaseNoOverlap, 2)
    );
    // Stall release and no-overlap both apply: the no-overlap reason wins.
    assert_eq!(
        decide(HoldAction::ReleaseStalled, &m, Some(&p), &c, &f).0,
        HoldAction::ReleaseNoOverlap
    );
}

#[test]
fn a_shared_file_keeps_the_hold() {
    let c = case();
    let m = soft(10, 9);
    let f = BTreeMap::from([(9, set(&["a.rs", "x.rs"])), (10, set(&["x.rs"]))]);
    assert_eq!(
        decide(HoldAction::HoldSoft, &m, Some(&pred_at(sha(9))), &c, &f).0,
        HoldAction::HoldSoft
    );
    assert_eq!(
        decide(HoldAction::ReleaseStalled, &m, Some(&pred_at(sha(9))), &c, &f).0,
        HoldAction::ReleaseStalled,
        "the stall release is untouched when the PRs do overlap"
    );
}

#[test]
fn the_release_comment_names_the_reason_and_the_counter_is_separate() {
    let body = release_comment_body(&soft(10, 9), HoldAction::ReleaseNoOverlap);
    assert!(body.contains("share no changed files"), "{body}");
    assert!(
        body.contains("#9") && body.contains("transitive-only") && body.contains("#10077"),
        "{body}"
    );
    assert!(body.starts_with("<!-- loom:sequence released plan=seq-0badc0de -->"), "{body}");
    let stats = super::super::MergeSequenceStats::default();
    assert_eq!(stats.overlap_released, 0);
}

// --- Fail closed ---------------------------------------------------------

#[test]
fn a_failed_files_read_on_either_side_keeps_the_hold() {
    let c = case();
    let m = soft(10, 9);
    let p = pred_at(sha(9));
    let only_pred = BTreeMap::from([(9, set(&["docs/a.md"]))]);
    let only_follower = BTreeMap::from([(10, set(&["src/b.rs"]))]);
    assert_eq!(
        decide(HoldAction::HoldSoft, &m, Some(&p), &c, &only_pred).0,
        HoldAction::HoldSoft
    );
    assert_eq!(
        decide(HoldAction::HoldSoft, &m, Some(&p), &c, &only_follower).0,
        HoldAction::HoldSoft
    );
    let empty = set(&[]);
    assert!(!no_overlap_release(&m, &c.follower, Some(&c.pred), None, Some(&empty)));
    assert!(!no_overlap_release(&m, &c.follower, Some(&c.pred), Some(&empty), None));
}

#[test]
fn a_predecessor_missing_from_the_listing_keeps_the_hold_without_reading_files() {
    let mut c = case();
    c.open.retain(|p| p.number != 9);
    let m = soft(10, 9);
    assert_eq!(
        decide(HoldAction::HoldSoft, &m, Some(&pred_at(sha(9))), &c, &disjoint()),
        (HoldAction::HoldSoft, 0)
    );
    assert!(!release_candidate(&m, &c.follower, None));
}

#[test]
fn a_hard_marker_or_a_consolidation_plan_is_never_released() {
    let c = case();
    let p = pred_at(sha(9));
    let hard = marker(10, 9, None, "manual");
    assert_eq!(
        decide(HoldAction::HoldHard, &hard, Some(&p), &c, &disjoint()),
        (HoldAction::HoldHard, 0)
    );
    assert!(!no_overlap_release(
        &hard,
        &c.follower,
        Some(&c.pred),
        disjoint().get(&10),
        disjoint().get(&9)
    ));
    // Even if a caller handed it HoldSoft, the marker itself is checked.
    assert_eq!(
        decide(HoldAction::HoldSoft, &hard, Some(&p), &c, &disjoint()),
        (HoldAction::HoldSoft, 0)
    );
    let cons = marker(10, 9, Some("pass"), "cons-ab12cd34");
    assert_eq!(
        decide(HoldAction::HoldSoft, &cons, Some(&p), &c, &disjoint()),
        (HoldAction::HoldSoft, 0)
    );
}

#[test]
fn a_stacked_edge_is_never_released_even_with_disjoint_files() {
    let mut c = case();
    c.follower.base_ref = c.pred.head_ref.clone();
    let m = soft(10, 9);
    assert_eq!(
        decide(HoldAction::HoldSoft, &m, Some(&pred_at(sha(9))), &c, &disjoint()),
        (HoldAction::HoldSoft, 0)
    );
    // An unknown branch name cannot prove the pair is not stacked.
    let mut unknown = case();
    unknown.open[0].head_ref.clear();
    assert_eq!(
        decide(HoldAction::HoldSoft, &m, Some(&pred_at(sha(9))), &unknown, &disjoint()).0,
        HoldAction::HoldSoft
    );
}

#[test]
fn moved_heads_and_other_outcomes_keep_their_precedence() {
    let c = case();
    let m = soft(10, 9);
    let f = disjoint();
    // Release / dissolve / void / expire / hard are passed through untouched.
    for a in [
        HoldAction::Release,
        HoldAction::ReleaseDissolved,
        HoldAction::VoidAndReplan,
        HoldAction::Expire,
        HoldAction::HoldHard,
    ] {
        assert_eq!(decide(a, &m, Some(&pred_at(sha(9))), &c, &f), (a, 0), "{a:?}");
    }
    // HoldSoft that is NOT in flight (unreadable or moved predecessor, moved
    // follower) never becomes a no-overlap release.
    assert_eq!(decide(HoldAction::HoldSoft, &m, None, &c, &f), (HoldAction::HoldSoft, 0));
    assert_eq!(
        decide(HoldAction::HoldSoft, &m, Some(&pred_at(sha(777))), &c, &f),
        (HoldAction::HoldSoft, 0)
    );
    let mut moved = case();
    moved.follower.head_sha = Some(sha(888));
    assert_eq!(
        decide(HoldAction::HoldSoft, &m, Some(&pred_at(sha(9))), &moved, &f),
        (HoldAction::HoldSoft, 0)
    );
    moved.follower.head_sha = None;
    assert_eq!(
        decide(HoldAction::HoldSoft, &m, Some(&pred_at(sha(9))), &moved, &f),
        (HoldAction::HoldSoft, 0)
    );
    // The real Phase-1 decision for a moved predecessor is a void.
    let p = pred_at(sha(777));
    let base =
        super::super::stall::hold_action_with_stall(&m, Some(&p), Some(&sha(10)), true, 72.0, None);
    assert_eq!(base, HoldAction::VoidAndReplan);
}

// --- Shared per-tick cache ------------------------------------------------

#[test]
fn the_tick_cache_reads_each_pr_once_and_remembers_a_failed_read() {
    let calls = Cell::new(0);
    let fetch = |p: &SequencePr| {
        calls.set(calls.get() + 1);
        (p.number == 1).then(|| set(&["a.rs"]))
    };
    let (a, b) = (pr(1, "2026-01-01T00:00:00Z", &[]), pr(2, "2026-01-02T00:00:00Z", &[]));
    let mut cache = TickFiles::default();
    assert_eq!(cache.get_or_fetch(&a, fetch), Some(&set(&["a.rs"])));
    assert_eq!(cache.get_or_fetch(&a, fetch), Some(&set(&["a.rs"])));
    assert_eq!(cache.get_or_fetch(&b, fetch), None);
    assert_eq!(cache.get_or_fetch(&b, fetch), None, "a failed read is not retried this tick");
    assert_eq!(calls.get(), 2, "at most one read per PR per tick");
    // The planner sees only successful reads, and only the PRs it asks for.
    let known = cache.known([1, 2, 3]);
    assert_eq!(known, BTreeMap::from([(1, set(&["a.rs"]))]));
}

// --- No churn --------------------------------------------------------------

fn edges(groups: &[SequenceGroup]) -> Vec<(u32, u32)> {
    groups
        .iter()
        .flat_map(|g| g.edges.iter().map(|e| (e.follower, e.after)))
        .collect()
}

#[test]
fn re_planning_after_a_release_never_re_creates_the_released_edge() {
    // #10061 was recorded after #10056 through #10053's files. After the
    // release the label is gone, so the follower has no marker and is
    // planned afresh: it may wait for a PR it really overlaps (#10060), never
    // for the disjoint one again.
    let prs = [
        pr(1, "2026-10-01T00:00:00Z", &["loom:pr"]),
        pr(2, "2026-10-01T01:00:00Z", &["loom:pr"]),
        pr(3, "2026-10-01T02:00:00Z", &["loom:pr"]),
    ];
    let files = BTreeMap::from([
        (1, set(&["docs/a.md", "lib.rs"])),
        (2, set(&["lib.rs", "src/x.rs"])),
        (3, set(&["src/x.rs"])),
    ]);
    let m = soft(3, 1);
    let c = Case {
        pred: prs[0].clone(),
        follower: prs[2].clone(),
        open: prs.to_vec(),
    };
    assert_eq!(
        decide(HoldAction::HoldSoft, &m, Some(&pred_at(sha(1))), &c, &files).0,
        HoldAction::ReleaseNoOverlap
    );
    let replan = || edges(&plan_repo_with(&prs, &files, &BTreeMap::new(), &BTreeSet::new()));
    let after = replan();
    assert!(!after.contains(&(3, 1)), "{after:?}");
    assert!(after.contains(&(3, 2)), "it waits for the PR it overlaps: {after:?}");
    // Re-planning a second time is stable (no oscillation).
    assert_eq!(after, replan());
    // Through the wall-clock planner too: whatever it orders, never (3, 1).
    assert!(!edges(&plan_repo(&prs, &files, &BTreeMap::new())).contains(&(3, 1)));
}

// --- Dry run ---------------------------------------------------------------

/// A fake `gh` answering from JSON files under `dir`, logging every call.
#[cfg(unix)]
fn fake_gh(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("gh.log");
    let bin = dir.join("fake-gh-overlap.sh");
    let d = dir.display();
    let script = format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{log}\"\n\
         case \"$1 $2\" in\n\
         'api '*/issues/*) n=\"${{2#*/issues/}}\"; n=\"${{n%%/*}}\"; cat \"{d}/comments-$n.json\" 2>/dev/null || echo '[]' ;;\n\
         'api '*/pulls/*) cat \"{d}/pull-${{2##*/}}.json\" || exit 1 ;;\n\
         'pr view') cat \"{d}/files-$3.json\" || exit 1 ;;\n\
         *) exit 1 ;;\nesac\n",
        log = log.display(),
    );
    std::fs::write(&bin, script).unwrap();
    let mut perms = std::fs::metadata(&bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).unwrap();
    (bin, log)
}

#[cfg(unix)]
fn write_json(dir: &std::path::Path, name: &str, v: &serde_json::Value) {
    std::fs::write(dir.join(name), v.to_string()).unwrap();
}

#[cfg(unix)]
fn comment(body: &str) -> serde_json::Value {
    serde_json::json!([{
        "body": body,
        "author_association": "OWNER",
        "user": {"login": "op", "type": "User"}
    }])
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn the_dry_run_lists_disjoint_holds_reads_once_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let d = dir.path();
    // #20 holds after #10 (disjoint) — would release. #21 holds after #10 but
    // shares a file — kept. #22 carries the label with no marker — a manual
    // hold, never read for files.
    let open = vec![
        pr(10, "2026-10-01T00:00:00Z", &[]),
        pr(20, "2026-10-02T00:00:00Z", &[SEQUENCE_LABEL]),
        pr(21, "2026-10-02T01:00:00Z", &[SEQUENCE_LABEL]),
        pr(22, "2026-10-02T02:00:00Z", &[SEQUENCE_LABEL]),
    ];
    write_json(d, "comments-20.json", &comment(&marker_text(&soft(20, 10))));
    write_json(d, "comments-21.json", &comment(&marker_text(&soft(21, 10))));
    let now = chrono::Utc::now().to_rfc3339();
    write_json(
        d,
        "pull-10.json",
        &serde_json::json!({"state": "open", "merged": false, "head": {"sha": sha(10)}, "updated_at": now}),
    );
    for (n, paths) in [
        (10, vec!["docs/a.md"]),
        (20, vec!["src/b.rs"]),
        (21, vec!["docs/a.md"]),
    ] {
        let rows: Vec<_> = paths
            .iter()
            .map(|p| serde_json::json!({"path": p}))
            .collect();
        write_json(d, &format!("files-{n}.json"), &serde_json::json!({ "files": rows }));
    }
    let (gh, log) = fake_gh(d);
    let mut cache = TickFiles::default();
    let out = would_release(&gh, &root, &open, &mut cache);
    assert_eq!(out, vec![(20, 10)]);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(!calls.contains("pr edit") && !calls.contains("pr comment"), "{calls}");
    for n in [10, 20, 21] {
        assert!(calls.matches(&format!("pr view {n} ")).count() <= 1, "{calls}");
    }
    assert!(!calls.contains("pr view 22 "), "a manual hold reads no files: {calls}");
}

// --- Property test ---------------------------------------------------------

/// SplitMix64: a tiny deterministic generator (no crate, #10077).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

const POOL: [&str; 6] = [
    "lib.rs",
    "cli/mod.rs",
    "a.rs",
    "b.rs",
    "docs/x.md",
    "Cargo.toml",
];

fn gen_case(rng: &mut Rng) -> (Vec<SequencePr>, BTreeMap<u32, BTreeSet<String>>) {
    let n = 3 + rng.below(10) as u32; // 3..=12
    let mut prs: Vec<SequencePr> = (1..=n)
        .map(|i| {
            let created = format!("2026-01-{:02}T{:02}:00:00Z", 1 + rng.below(3), rng.below(24));
            let labels: &[&str] = if rng.chance(30) { &["loom:pr"] } else { &[] };
            pr(100 + i, &created, labels)
        })
        .collect();
    for i in 0..prs.len() {
        if rng.chance(15) {
            let j = rng.below(prs.len() as u64) as usize;
            if j != i {
                prs[i].base_ref = prs[j].head_ref.clone();
            }
        }
    }
    let mut files = BTreeMap::new();
    for p in &prs {
        if rng.chance(5) {
            continue; // a failed read: no entry
        }
        let k = 1 + rng.below(3);
        let s: BTreeSet<String> = (0..k)
            .map(|_| POOL[rng.below(POOL.len() as u64) as usize].to_string())
            .collect();
        files.insert(p.number, s);
    }
    (prs, files)
}

/// Check one case; `Ok` carries the number of `(SharedFiles, StackedBase)`
/// edges seen, so the test can prove it was not vacuous.
fn check(
    prs: &[SequencePr],
    files: &BTreeMap<u32, BTreeSet<String>>,
) -> Result<(usize, usize), String> {
    let mut seen = (0, 0);
    let by: BTreeMap<u32, &SequencePr> = prs.iter().map(|p| (p.number, p)).collect();
    for g in plan_repo(prs, files, &BTreeMap::new()) {
        for e in &g.edges {
            if e.follower == e.after {
                return Err(format!("self-predecessor {e:?}"));
            }
            let (f, p) = (by[&e.follower], by[&e.after]);
            if !super::super::ready::ready(p) {
                return Err(format!("edge behind a non-ready predecessor {e:?} (#10371)"));
            }
            match e.reason {
                EdgeReason::SharedFiles => {
                    let shared = files
                        .get(&e.follower)
                        .zip(files.get(&e.after))
                        .is_some_and(|(a, b)| !a.is_disjoint(b));
                    if !shared {
                        return Err(format!("SharedFiles edge between disjoint PRs {e:?}"));
                    }
                    seen.0 += 1;
                }
                EdgeReason::StackedBase => {
                    if f.base_ref != p.head_ref {
                        return Err(format!("StackedBase edge without stacking {e:?}"));
                    }
                    seen.1 += 1;
                }
            }
        }
    }
    Ok(seen)
}

#[test]
fn seeded_property_every_edge_joins_directly_related_prs() {
    const SEED: u64 = 0x1007_7a11_5eed_0001;
    const CASES: usize = 600;
    let mut rng = Rng(SEED);
    let mut total = (0, 0);
    for i in 0..CASES {
        let (prs, files) = gen_case(&mut rng);
        match check(&prs, &files) {
            Ok((shared, stacked)) => total = (total.0 + shared, total.1 + stacked),
            Err(why) => panic!("seed {SEED:#x} case {i}: {why}\nprs: {prs:#?}\nfiles: {files:#?}"),
        }
    }
    assert!(
        total.0 > 0 && total.1 > 0,
        "the generator must exercise both edge kinds: {total:?}"
    );
}
