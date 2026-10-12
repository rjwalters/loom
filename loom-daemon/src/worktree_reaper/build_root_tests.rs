//! #11193: agent-chosen cargo target dirs (`.loom-target/`,
//! `.loom/target-doctor-<N>/`, ...) in kept and merged worktrees.
//!
//! Two decisions are pinned separately: the `pr-<N>` removal classifier (a
//! proven build tree no longer counts as untracked user work, and every other
//! gate still holds), and the artifact-only reclaim (a proven, owned, idle,
//! contained, untracked root goes; anything else stays with a reason).
//! `#[serial]` because `enumerate_worktree_dirs` reads `LOOM_WORKTREE_ROOT`.

use super::*;
use crate::generated_artifact::build_tree::tests::{cargo_tree, mixed_tree, seeded_repo, write};
use crate::worktree_ops::clean::{classify_pr_worktree, production_pr_probes, PrWorktreeProbes};
use crate::worktree_ops::safety::InUseMarker;
use serial_test::serial;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;

fn no_handles(_: &Path) -> Option<bool> {
    Some(false)
}

fn no_executables(_: &Path) -> Vec<LiveExecutable> {
    Vec::new()
}

fn one_volume(_: &Path) -> Option<u64> {
    Some(1)
}

fn probes<'a>(
    open_handles: &'a dyn Fn(&Path) -> Option<bool>,
    executing_within: &'a dyn Fn(&Path) -> Vec<LiveExecutable>,
) -> ArtifactProbes<'a> {
    ArtifactProbes {
        open_handles,
        executing_within,
        volume: &one_volume,
        euid: current_euid(),
    }
}

fn open_issue(_: &Path, _: u32) -> WorktreeDecision {
    WorktreeDecision::SkipIssueNotClosed("OPEN".to_string())
}

fn open_pr(_: &Path, _: u32) -> WorktreeDecision {
    WorktreeDecision::SkipPrOpen
}

fn class<'a>(
    prefix: &'static str,
    classify: &'a dyn Fn(&Path, u32) -> WorktreeDecision,
) -> WorktreeClass<'a> {
    let parse_name: &dyn Fn(&str) -> Option<u32> = if prefix == "issue" {
        &crate::worktree_ops::naming::issue_from_worktree
    } else {
        &crate::worktree_ops::naming::pr_from_worktree
    };
    WorktreeClass {
        prefix,
        parse_name,
        classify,
    }
}

/// A Loom-managed git worktree at `<root>/.loom/worktrees/<name>`.
fn managed_worktree(root: &Path, name: &str) -> PathBuf {
    let wt = root.join(".loom/worktrees").join(name);
    fs::create_dir_all(&wt).unwrap();
    seeded_repo(&wt);
    write(&wt.join(".loom-managed"), b"");
    wt.canonicalize().unwrap()
}

fn reasons(report: &ReclaimReport) -> BTreeMap<String, String> {
    report
        .kept
        .iter()
        .map(|(a, why)| (a.name.clone(), why.clone()))
        .collect()
}

#[test]
#[serial]
fn both_kept_classes_reclaim_proven_build_roots_regardless_of_name() {
    for (prefix, name, classify) in [
        ("issue", "issue-7", &open_issue as &dyn Fn(&Path, u32) -> WorktreeDecision),
        ("pr", "pr-8", &open_pr),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let wt = managed_worktree(tmp.path(), name);
        cargo_tree(&wt.join(".loom/target-doctor-1"));
        cargo_tree(&wt.join(".loom-target"));
        mixed_tree(&wt.join(".cargo-target-x"));
        write(&wt.join("target-plain/debug/x.o"), b"untagged");
        write(&wt.join("src/lib.rs"), b"pub fn f() {}");

        let report = reclaim_kept(
            tmp.path(),
            &class(prefix, classify),
            false,
            Duration::ZERO,
            &probes(&no_handles, &no_executables),
            &never_stop,
        );

        let removed: Vec<&str> = report.removed.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(removed.len(), 2, "{prefix}: {removed:?}");
        assert!(removed.contains(&".loom-target") && removed.contains(&".loom/target-doctor-1"));
        assert!(report
            .removed
            .iter()
            .all(|a| a.class == prefix && a.bytes > 0));
        assert!(!wt.join(".loom-target").exists() && !wt.join(".loom/target-doctor-1").exists());
        assert!(wt.join(".loom").is_dir(), "only the build root goes, not its parent");
        // Mixed content survives with an explicit reason; untagged is ignored.
        assert!(wt.join(".cargo-target-x/notes.rs").exists());
        let why = &reasons(&report)[".cargo-target-x"];
        assert!(why.contains("not a proven cargo build tree"), "{why}");
        assert!(wt.join("target-plain/debug/x.o").exists());
        assert!(wt.join("src/lib.rs").exists() && wt.join("tracked.txt").exists());
    }
}

#[test]
#[serial]
fn unsafe_roots_are_kept_with_an_explicit_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = managed_worktree(tmp.path(), "issue-9");
    let outside = tmp.path().join("outside-tree");
    cargo_tree(&outside);
    symlink(&outside, wt.join(".loom-target")).unwrap();
    write(
        &wt.join("vendored-target/CACHEDIR.TAG"),
        crate::generated_artifact::build_tree::tests::SIG.as_bytes(),
    );
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(&wt)
        .args(["add", "vendored-target"])
        .status()
        .unwrap()
        .success();
    assert!(ok);
    cargo_tree(&wt.join("vendored-target"));

    let report = reclaim_kept(
        tmp.path(),
        &class("issue", &open_issue),
        false,
        Duration::ZERO,
        &probes(&no_handles, &no_executables),
        &never_stop,
    );
    assert!(report.removed.is_empty(), "{:?}", report.removed);
    let why = reasons(&report);
    assert!(why[".loom-target"].contains("symlinked"), "{why:?}");
    assert!(why["vendored-target"].contains("git tracks"), "{why:?}");
    assert!(outside.join("debug/deps/libx-1.rlib").exists(), "symlink target untouched");

    // A root owned by another uid is kept (simulated via the euid probe).
    let mut report = ReclaimReport::default();
    fs::remove_file(wt.join(".loom-target")).unwrap();
    cargo_tree(&wt.join(".loom-target"));
    let candidates = collect(
        tmp.path(),
        &class("issue", &open_issue),
        SystemTime::now(),
        Duration::ZERO,
        current_euid() + 1,
        &mut report,
    );
    assert!(candidates.is_empty(), "{candidates:?}");
    assert!(reasons(&report)[".loom-target"].contains("owned by uid"));
}

#[test]
#[serial]
fn open_handles_and_executing_binaries_keep_a_proven_root() {
    let held = |_: &Path| Some(true);
    let executing = |p: &Path| {
        vec![LiveExecutable {
            pid: 4242,
            exe: p.join("debug/x"),
        }]
    };
    for (open, exec, want) in [
        (
            &held as &dyn Fn(&Path) -> Option<bool>,
            &no_executables as &dyn Fn(&Path) -> Vec<LiveExecutable>,
            "open",
        ),
        (&no_handles, &executing, "4242"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let wt = managed_worktree(tmp.path(), "pr-11");
        cargo_tree(&wt.join(".loom/target-doctor-11"));
        let report = reclaim_kept(
            tmp.path(),
            &class("pr", &open_pr),
            false,
            Duration::ZERO,
            &probes(open, exec),
            &never_stop,
        );
        assert!(report.removed.is_empty());
        assert!(wt.join(".loom/target-doctor-11/debug/x").exists());
        let why = &reasons(&report)[".loom/target-doctor-11"];
        assert!(why.contains(want), "{why}");
    }
}

#[test]
#[serial]
fn a_skipped_worktree_reports_its_discovered_roots_with_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = managed_worktree(tmp.path(), "issue-12");
    cargo_tree(&wt.join(".loom-target"));
    let busy = |_: &Path, _: u32| WorktreeDecision::SkipInUse("busy".to_string());
    let report = reclaim_kept(
        tmp.path(),
        &class("issue", &busy),
        false,
        Duration::ZERO,
        &probes(&no_handles, &no_executables),
        &never_stop,
    );
    assert!(wt.join(".loom-target").exists());
    let (artifact, why) = &report.kept[0];
    assert_eq!((artifact.name.as_str(), why.as_str()), (".loom-target", "busy"));
    assert!(artifact.bytes > 0);
}

// ----------------------------------------------------------------------------
// The `pr-<N>` removal classifier, with the production `uncommitted` and
// `is_managed` probes and every other gate scripted.
// ----------------------------------------------------------------------------

struct Gates {
    procs: Vec<u32>,
    editables: Vec<String>,
    in_use: bool,
    status: PrStatus,
}

fn merged(minutes_ago: i64) -> PrStatus {
    PrStatus::Merged {
        merged_at: (Utc::now() - chrono::Duration::minutes(minutes_ago)).to_rfc3339(),
    }
}

impl Default for Gates {
    fn default() -> Self {
        Self {
            procs: Vec::new(),
            editables: Vec::new(),
            in_use: false,
            status: merged(60),
        }
    }
}

fn decide(wt: &Path, gates: &Gates) -> WorktreeDecision {
    let opts = crate::worktree_reaper::reaper_clean_options(600);
    let in_use = |_: &Path| gates.in_use.then(InUseMarker::default);
    let procs = |_: &Path| gates.procs.clone();
    let editables = |_: &Path| gates.editables.clone();
    let status = |_: u32| gates.status.clone();
    let reachable = |_: &Path| true;
    let probes = PrWorktreeProbes {
        in_use_marker: &in_use,
        processes_using: &procs,
        editable_installs: &editables,
        ..production_pr_probes(&status, &reachable, Utc::now())
    };
    classify_pr_worktree(wt, 8, &opts, &probes)
}

#[test]
fn the_pr_classifier_removes_a_merged_worktree_holding_only_proven_build_output() {
    type Setup = fn(&Path);
    let none: Setup = |_| {};
    let cases: Vec<(&str, Setup, Gates, WorktreeDecision)> = vec![
        ("only a proven build tree", none, Gates::default(), WorktreeDecision::Remove),
        (
            "no .loom-managed",
            |wt| fs::remove_file(wt.join(".loom-managed")).unwrap(),
            Gates::default(),
            WorktreeDecision::SkipUnmanaged,
        ),
        (
            "active process",
            none,
            Gates {
                procs: vec![42],
                ..Gates::default()
            },
            WorktreeDecision::SkipInUse(String::new()),
        ),
        (
            "editable install",
            none,
            Gates {
                editables: vec!["pkg".into()],
                ..Gates::default()
            },
            WorktreeDecision::SkipEditable(String::new()),
        ),
        (
            ".loom-in-use",
            none,
            Gates {
                in_use: true,
                ..Gates::default()
            },
            WorktreeDecision::SkipInUse(String::new()),
        ),
        (
            "tracked change",
            |wt| write(&wt.join("tracked.txt"), b"edited"),
            Gates::default(),
            WorktreeDecision::SkipUncommitted,
        ),
        (
            "ordinary untracked .rs",
            |wt| write(&wt.join("scratch.rs"), b"fn f() {}"),
            Gates::default(),
            WorktreeDecision::SkipUncommitted,
        ),
        (
            "tagged dir with user source",
            |wt| mixed_tree(&wt.join(".loom-target")),
            Gates::default(),
            WorktreeDecision::SkipUncommitted,
        ),
        (
            "truncated signature",
            |wt| {
                write(&wt.join("t/CACHEDIR.TAG"), b"Signature: 8a47");
                write(&wt.join("t/debug/x.d"), b"d");
            },
            Gates::default(),
            WorktreeDecision::SkipUncommitted,
        ),
        (
            "PR open",
            none,
            Gates {
                status: PrStatus::Open,
                ..Gates::default()
            },
            WorktreeDecision::SkipPrOpen,
        ),
        (
            "within grace",
            none,
            Gates {
                status: merged(1),
                ..Gates::default()
            },
            WorktreeDecision::SkipGrace(0),
        ),
        (
            "PR status unknown",
            none,
            Gates {
                status: PrStatus::Unknown,
                ..Gates::default()
            },
            WorktreeDecision::SkipUnknownPrStatus,
        ),
        (
            "no PR",
            none,
            Gates {
                status: PrStatus::NoPr,
                ..Gates::default()
            },
            WorktreeDecision::SkipNotMerged(String::new()),
        ),
    ];
    for (name, setup, gates, want) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();
        seeded_repo(wt);
        write(&wt.join(".loom-managed"), b"");
        cargo_tree(&wt.join(".loom/target-doctor-1"));
        setup(wt);
        let got = decide(wt, &gates);
        assert_eq!(std::mem::discriminant(&got), std::mem::discriminant(&want), "{name}: {got:?}");
    }
}
