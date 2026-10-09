//! Size and scope predictors (#10960): path categories, the diff stat and
//! huge-PR rule, merged-PR churn; point-in-time discipline (a list read at or
//! after `as_of`, a later incomplete read, a merge after `as_of`); fit/serve
//! parity; and the drift guard pinning [`CRITICAL_PATTERNS`] to the
//! Champion's prompt.

use super::fit_rows::{approve, cutoff, h, landed, open, secs, snapshot, REPO};
use super::provenance;
use crate::eta::episodes::{EpisodeEnd, EpisodeNext, StageEpisode};
use crate::eta::fit::features_v3::FEATURES_V3;
use crate::eta::fit::rows;
use crate::eta::fit::KNOWABLE_LAG_SEC;
use crate::eta::loop_features::{FileSnapshot, LOOP_FEATURES};
use crate::eta::scope_features::{
    churn_context, is_critical, is_docs, is_test, path_flags, scope_features, scope_vector,
    PathFlags, ScopeCoverage, ScopeFeatures, ScopeInputs, CRITICAL_PATTERNS, N_SCOPE_FEATURES,
    SCOPE_FEATURES,
};
use crate::eta::tracker::Tracker;
use crate::eta::Stage;
use crate::pr_latency::history::fixtures::labeled;
use crate::pr_latency::REVIEW_REQUESTED;
use chrono::{DateTime, Duration, Utc};

fn at(x: f64) -> DateTime<Utc> {
    h(x)
}

fn lag() -> Duration {
    Duration::seconds(KNOWABLE_LAG_SEC)
}

fn snap(pr: u32, known: f64, files: &[&str], stat: Option<(u32, u32)>) -> FileSnapshot {
    FileSnapshot {
        repo: REPO.to_string(),
        pr,
        known_at: at(known),
        files: files.iter().map(|f| (*f).to_string()).collect(),
        head_sha: None,
        complete: true,
        additions: stat.map(|s| s.0),
        deletions: stat.map(|s| s.1),
        listed: Some(u32::try_from(files.len()).unwrap()),
    }
}

/// An incomplete read that listed `listed` entries.
fn incomplete(pr: u32, known: f64, listed: u32) -> FileSnapshot {
    FileSnapshot {
        files: vec![],
        complete: false,
        listed: Some(listed),
        additions: None,
        deletions: None,
        ..snap(pr, known, &[], None)
    }
}

fn merged(pr: u32, entered: f64, at_h: f64) -> StageEpisode {
    StageEpisode {
        repo: REPO.to_string(),
        pr_number: pr,
        stage: Stage::MergeWait,
        entered_at: at(entered),
        end: EpisodeEnd::Left {
            at: at(at_h),
            next: EpisodeNext::Merged,
        },
    }
}

fn compute(eps: &[StageEpisode], files: Option<&[FileSnapshot]>, pr: u32, t: f64) -> ScopeFeatures {
    let refs: Vec<&StageEpisode> = eps.iter().collect();
    scope_features(
        &ScopeInputs {
            repo: REPO,
            pr,
            repo_episodes: &refs,
            files,
        },
        at(t),
    )
}

// -- path categories ----------------------------------------------------------

#[test]
fn path_categories_follow_the_documented_table() {
    for p in [
        "README.md",
        "docs/eta.md",
        "docs/img/chart.svg",
        "notes/todo.txt",
        "A.MD",
    ] {
        assert!(is_docs(p), "{p} is docs");
    }
    for p in ["src/docs.rs", "defaults/docs/eta.rs", "mdbook.toml"] {
        assert!(!is_docs(p), "{p} is not docs");
    }
    for p in [
        "loom-daemon/src/eta/tests/scope_features.rs",
        "tests/e2e.rs",
        "src/eta/doctor_tests.rs",
        "src/app.test.ts",
        "defaults/scripts/tests/test-champion-critical-file-check.sh",
        "scripts/test-foo.sh",
    ] {
        assert!(is_test(p), "{p} is a test");
    }
    for p in [
        "src/tests.rs",
        "src/testing/a.rs",
        "scripts/test-foo.py",
        "tests",
    ] {
        assert!(!is_test(p), "{p} is not a test");
    }
    assert!(is_critical("loom-daemon/Cargo.toml"));
    assert!(is_critical(".github/workflows/ci.yml"));
    assert!(is_critical("db/migrations/0001.sql"));
    assert!(is_critical("app/add_user_migration.py"));
    assert!(
        !is_critical("docs/migration/v0.10.md"),
        "#5723: a bare 'migration' is not critical"
    );
    assert!(!is_critical("defaults/observability/q.sql"), "#9357: no bare .sql");
}

#[test]
fn flags_need_every_path_for_only_and_one_for_touches() {
    let paths = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    let docs = path_flags(&paths(&["README.md", "docs/a.md"]));
    assert!(docs.docs_only && !docs.tests_only && !docs.touches_rust);
    let mixed = path_flags(&paths(&["README.md", "src/a.rs", "ui/x.tsx", "s/run.sh"]));
    assert_eq!(
        mixed,
        PathFlags {
            docs_only: false,
            tests_only: false,
            touches_rust: true,
            touches_ts: true,
            touches_shell: true,
            touches_critical: false,
        }
    );
    let tests = path_flags(&paths(&["src/tests/a.rs", "src/b_tests.rs"]));
    assert!(tests.tests_only && tests.touches_rust && !tests.docs_only);
    // An empty list is known and claims nothing.
    assert_eq!(path_flags(&[]), PathFlags::default());
}

// -- the builder ------------------------------------------------------------

#[test]
fn no_log_or_no_read_is_unknown_never_small() {
    let files = vec![snap(1, 1.0, &["a.rs"], Some((3, 4)))];
    let none = compute(&[], None, 1, 5.0);
    assert_eq!(none, ScopeFeatures::default());
    // Before the first read the list is unknown.
    let early = compute(&[], Some(&files), 1, 0.5);
    assert_eq!(early, ScopeFeatures::default());
    let v = scope_vector(&early);
    assert!(v.iter().all(|x| *x == 0.0), "unknown is all-zero with every indicator 0");
}

#[test]
fn a_whole_list_gives_size_flags_and_churn() {
    let files = vec![
        snap(1, 1.0, &["a.rs", "Cargo.toml", "docs/x.md"], Some((30, 10))),
        snap(90, 0.5, &["a.rs", "z.rs"], Some((1, 1))),
    ];
    let eps = vec![merged(90, 1.0, 2.0)];
    let f = compute(&eps, Some(&files), 1, 5.0);
    assert_eq!(f.files, Some(3));
    assert_eq!(f.lines, Some(40));
    assert_eq!(f.huge, Some(false));
    let p = f.paths.expect("whole list");
    assert!(p.touches_rust && p.touches_critical && !p.docs_only);
    assert_eq!(f.churn_7d, Some(1), "only a.rs is shared with the merged PR");
    let v = scope_vector(&f);
    assert_eq!(v.len(), N_SCOPE_FEATURES);
    assert!((v[0] - 41_f64.ln()).abs() < 1e-12);
    assert_eq!(v[1], 1.0);
    assert!((v[2] - 4_f64.ln()).abs() < 1e-12);
    assert_eq!(v[4], 1.0);
    assert_eq!(v[10], 1.0);
    assert!((v[11] - 2_f64.ln()).abs() < 1e-12);
    assert_eq!(v[12], 1.0);
}

#[test]
fn a_full_page_is_huge_with_paths_unknown() {
    let files = vec![
        snap(1, 1.0, &["a.md"], Some((1, 0))),
        incomplete(1, 2.0, 100),
    ];
    let f = compute(&[], Some(&files), 1, 3.0);
    assert_eq!(f.huge, Some(true));
    assert_eq!(f.listed, Some(100));
    assert!(f.paths.is_none() && f.files.is_none() && f.lines.is_none() && f.churn_7d.is_none());
    let v = scope_vector(&f);
    assert!((v[2] - 101_f64.ln()).abs() < 1e-12, "a huge PR's size is a lower bound, not 0");
    assert_eq!(v[3], 1.0);
    assert_eq!(v[4], 0.0, "scope_known = 0");
    // Before the incomplete read, the older whole list still serves.
    let before = compute(&[], Some(&files), 1, 1.5);
    assert_eq!(before.paths.map(|p| p.docs_only), Some(true));
}

#[test]
fn a_head_inconsistent_page_is_unknown_and_not_huge() {
    let files = vec![snap(1, 1.0, &["a.md"], None), incomplete(1, 2.0, 3)];
    let f = compute(&[], Some(&files), 1, 3.0);
    assert_eq!(f.huge, Some(false));
    assert!(f.paths.is_none());
    assert_eq!(scope_vector(&f)[4], 0.0);
}

#[test]
fn an_old_line_without_the_stat_knows_paths_but_not_lines() {
    let line =
        r#"{"repo":"rjwalters/loom","pr":1,"known_at":"2026-10-07T00:00:00Z","files":["a.rs"]}"#;
    let old: FileSnapshot = serde_json::from_str(line).unwrap();
    let as_of = old.known_at + Duration::hours(1);
    let f = scope_features(
        &ScopeInputs {
            repo: REPO,
            pr: 1,
            repo_episodes: &[],
            files: Some(std::slice::from_ref(&old)),
        },
        as_of,
    );
    assert_eq!(f.files, Some(1));
    assert!(f.lines.is_none() && f.listed.is_none());
    assert_eq!(f.huge, Some(false), "a whole list is shorter than a page");
    assert_eq!(f.churn_7d, Some(0));
}

#[test]
fn a_list_read_at_or_after_as_of_changes_nothing() {
    let files = vec![snap(1, 1.0, &["a.rs"], Some((1, 1)))];
    let base = compute(&[], Some(&files), 1, 3.0);
    let mut later = files.clone();
    later.push(snap(1, 3.0, &["a.rs", "b.md", "c.sh"], Some((9, 9))));
    later.push(incomplete(1, 4.0, 100));
    assert_eq!(compute(&[], Some(&later), 1, 3.0), base);
}

#[test]
fn churn_reads_a_merge_only_after_it_and_an_unknown_peer_list_as_unknown() {
    let files = vec![
        snap(1, 1.0, &["a.rs", "b.rs"], None),
        snap(90, 0.5, &["a.rs"], None),
        snap(91, 0.5, &["b.rs"], None),
    ];
    let eps = vec![merged(90, 1.0, 2.0), merged(91, 1.0, 6.0)];
    // #91 has not merged yet at 5 h: its list does not count.
    assert_eq!(compute(&eps, Some(&files), 1, 5.0).churn_7d, Some(1));
    assert_eq!(
        compute(&eps, Some(&files), 1, 6.0).churn_7d,
        Some(1),
        "a merge at as_of is not before it"
    );
    assert_eq!(compute(&eps, Some(&files), 1, 7.0).churn_7d, Some(2));
    // Past the 7-day window both drop out.
    assert_eq!(compute(&eps, Some(&files), 1, 6.0 + 7.0 * 24.0 + 1.0).churn_7d, Some(0));
    // A merged PR with no list known before as_of leaves churn unknown.
    let eps = vec![merged(90, 1.0, 2.0), merged(92, 1.0, 3.0)];
    let f = compute(&eps, Some(&files), 1, 5.0);
    assert!(f.churn_7d.is_none() && f.paths.is_some());
    // ...and so does one whose last read was incomplete.
    let mut files2 = files.clone();
    files2.push(incomplete(92, 0.5, 100));
    assert!(compute(&eps, Some(&files2), 1, 5.0).churn_7d.is_none());
    // The subject's own merge episodes (another stage visit) never count.
    let own = vec![merged(1, 1.0, 2.0)];
    assert_eq!(compute(&own, Some(&files), 1, 5.0).churn_7d, Some(0));
}

#[test]
fn the_churn_context_equals_the_whole_history() {
    let files = vec![
        snap(1, 1.0, &["a.rs"], None),
        snap(90, 0.5, &["a.rs"], None),
    ];
    let eps = [
        merged(90, 1.0, 2.0),
        StageEpisode {
            repo: REPO.to_string(),
            pr_number: 2,
            stage: Stage::ReviewWait,
            entered_at: at(1.0),
            end: EpisodeEnd::Open { at: at(100.0) },
        },
    ];
    let refs: Vec<&StageEpisode> = eps.iter().collect();
    for t in [1.5, 3.0, 200.0] {
        let context = churn_context(&refs, at(t));
        let inputs = |repo_episodes| ScopeInputs {
            repo: REPO,
            pr: 1,
            repo_episodes,
            files: Some(&files),
        };
        assert_eq!(scope_features(&inputs(&refs), at(t)), scope_features(&inputs(&context), at(t)));
    }
}

#[test]
fn candidate_names_are_new_columns() {
    assert_eq!(SCOPE_FEATURES.len(), N_SCOPE_FEATURES);
    for name in SCOPE_FEATURES {
        assert!(!FEATURES_V3.contains(&name), "{name} must not be in eta-fit/v3");
        assert!(!LOOP_FEATURES.contains(&name), "{name} clashes with a friction predictor");
    }
}

#[test]
fn coverage_counts_each_input() {
    let rows = vec![
        ScopeFeatures::default(),
        ScopeFeatures {
            huge: Some(true),
            listed: Some(100),
            ..ScopeFeatures::default()
        },
        ScopeFeatures {
            files: Some(1),
            lines: Some(2),
            huge: Some(false),
            listed: Some(1),
            paths: Some(PathFlags::default()),
            churn_7d: Some(0),
        },
    ];
    let c = ScopeCoverage::of(&rows);
    assert_eq!((c.rows, c.scope_known, c.lines_known, c.huge, c.churn_known), (3, 1, 1, 1, 1));
}

// -- fit rows and serving ---------------------------------------------------

/// #1 and #2 open from 1-2 h; #90 merged at 3 h, #91 at 7 h.
fn fleet() -> Vec<crate::eta::fleet::FleetSnapshot> {
    let prs = vec![
        landed(90, h(1.0), h(2.0), h(3.0)),
        landed(91, h(1.0), h(2.0), h(7.0)),
        open(1, approve(h(1.0), h(3.0))),
        open(2, vec![labeled(REVIEW_REQUESTED, secs(h(2.0)))]),
    ];
    vec![snapshot(REPO, &prs, cutoff() + Duration::hours(1))]
}

fn logged() -> Vec<FileSnapshot> {
    vec![
        snap(1, 0.5, &["a.rs", "docs/x.md", "Cargo.toml"], Some((12, 3))),
        snap(2, 0.5, &["README.md"], Some((1, 0))),
        snap(90, 0.5, &["a.rs"], Some((2, 2))),
        snap(91, 0.5, &["docs/x.md"], Some((5, 0))),
    ]
}

fn row_of(a: &rows::Assembled, pr: u32, t: DateTime<Utc>) -> usize {
    a.row_keys
        .iter()
        .position(|k| k.at == t && k.pr == pr && k.repo == REPO)
        .expect("row")
}

#[test]
fn fit_rows_and_serving_share_one_builder() {
    let snaps = fleet();
    let files = logged();
    let a = rows::build_with_files(&snaps, cutoff(), None, None, Some(&files));
    assert_eq!(a.scope.len(), a.rows.len());
    let i = row_of(&a, 1, h(10.0));
    assert_eq!(a.scope[i].churn_7d, Some(2), "both merges are before the cutoff");
    assert_eq!(a.scope[i].lines, Some(15));
    assert!(a.scope[i].paths.unwrap().touches_critical);
    let j = row_of(&a, 2, h(10.0));
    assert!(a.scope[j].paths.unwrap().docs_only);
    assert_eq!(a.scope[row_of(&a, 1, h(5.0))].churn_7d, Some(1), "#91 merges at 7 h");

    // Without a file log every row is unknown, and the rows themselves agree.
    let none = rows::build(&snaps, cutoff());
    assert_eq!(none.rows, a.rows);
    assert!(none.scope.iter().all(|s| *s == ScopeFeatures::default()));

    let mut tracker = Tracker::new(provenance());
    tracker.on_fleet_snapshots(&snaps, h(10.0));
    assert!(tracker.scope_features_of(REPO, 1, h(10.0)).is_none(), "no log loaded: absent");
    tracker.set_file_snapshots(Some(files));
    for (k, s) in a.row_keys.iter().zip(&a.scope) {
        assert_eq!(
            tracker.scope_features_of(&k.repo, k.pr, k.at).as_ref(),
            Some(s),
            "#{} at {}",
            k.pr,
            k.at
        );
        assert_eq!(
            scope_vector(&tracker.scope_features_of(&k.repo, k.pr, k.at).unwrap()),
            scope_vector(s)
        );
    }
    assert_eq!(
        tracker
            .scope_features_of(&REPO.to_ascii_uppercase(), 1, h(10.0))
            .as_ref(),
        Some(&a.scope[i])
    );
    let c = ScopeCoverage::of(&a.scope);
    assert!(c.scope_known > 0 && c.churn_known > 0 && c.lines_known > 0);
}

#[test]
fn a_later_read_or_merge_moves_no_earlier_row() {
    let snaps = fleet();
    let base_files = logged();
    let base = rows::build_with_files(&snaps, cutoff(), None, None, Some(&base_files));
    // #1 grows past a page at 9 h: rows at or before then must not move;
    // later ones become huge with paths unknown, never small.
    let mut grown = base_files.clone();
    grown.push(incomplete(1, 9.0, 100));
    let after = rows::build_with_files(&snaps, cutoff(), None, None, Some(&grown));
    let (mut before, mut huge) = (0, 0);
    for ((k, b), a) in base.row_keys.iter().zip(&base.scope).zip(&after.scope) {
        if k.at - lag() <= at(9.0) {
            assert_eq!(b, a, "#{} at {} must not see the later read", k.pr, k.at);
            before += 1;
        } else if k.pr == 1 {
            assert!(
                a.paths.is_none() && a.huge == Some(true),
                "#1 at {} served a stale list",
                k.at
            );
            huge += 1;
        } else {
            assert_eq!(b, a);
        }
    }
    assert!(before > 0 && huge > 0, "the test must cover both sides of the read");
    let mut tracker = Tracker::new(provenance());
    tracker.on_fleet_snapshots(&snaps, h(10.0));
    tracker.set_file_snapshots(Some(grown));
    for (k, s) in after.row_keys.iter().zip(&after.scope) {
        assert_eq!(tracker.scope_features_of(&k.repo, k.pr, k.at).as_ref(), Some(s));
    }

    // #91's list does not count toward churn before its 7 h merge.
    for (k, s) in base.row_keys.iter().zip(&base.scope) {
        if k.pr == 1 && k.at - lag() < at(7.0) && k.at - lag() > at(3.0) {
            assert_eq!(s.churn_7d, Some(1), "#1 at {}", k.at);
        }
    }
}

// -- drift guard --------------------------------------------------------------

/// The quoted entries of the first `CRITICAL_PATTERNS=(` ... `)` array in
/// `text`, comments skipped.
fn bash_array(text: &str) -> Vec<String> {
    let mut lines = text
        .lines()
        .skip_while(|l| l.trim() != "CRITICAL_PATTERNS=(")
        .skip(1);
    let mut out = Vec::new();
    for l in lines.by_ref() {
        let l = l.trim();
        if l == ")" {
            return out;
        }
        if let Some(rest) = l.strip_prefix('"') {
            out.push(rest.trim_end_matches('"').to_string());
        }
    }
    panic!("unterminated CRITICAL_PATTERNS array");
}

#[test]
fn critical_patterns_match_the_champion_prompt() {
    for rel in [
        "defaults/.claude/commands/loom/champion-pr-merge.md",
        "defaults/scripts/tests/test-champion-critical-file-check.sh",
    ] {
        let path = format!("{}/../{rel}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let parsed = bash_array(&text);
        assert_eq!(
            parsed, CRITICAL_PATTERNS,
            "{rel}'s CRITICAL_PATTERNS drifted from scope_features"
        );
    }
}
