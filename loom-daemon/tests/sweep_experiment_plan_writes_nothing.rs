//! `loom-daemon sweep-experiment plan` must write **nothing** (issue #8244,
//! phase 1 of #8055).
//!
//! The pure assignment function is unit-tested next to itself in
//! `script_helpers::fleet_experiment::tests` (determinism, seed sensitivity,
//! per-stratum balance). Those tests cannot see the property this file exists
//! for: `plan` is the one command in the `plan` / `start` / `stop` trio that an
//! operator is expected to run casually — against a live fleet, before deciding
//! anything — so it must be safe to run at any time. "Pure function" is not the
//! same claim as "the command touched no file": the command also reads the
//! registry, measures each workspace, and renders. Any of those steps could
//! grow a cache file, a state stamp, or an `~/.loom/experiments/` entry without
//! a single test failing.
//!
//! So this file asserts the property where it is actually observable — at the
//! process boundary, over the whole filesystem subtree the command is pointed
//! at. Everything is confined to a temp fixture: a fixture registry
//! (`LOOM_WORKSPACES_PATH`), a fixture `HOME`, a fixture experiments dir
//! (`LOOM_EXPERIMENTS_DIR`), and fixture workspace roots. `--offline` keeps the
//! `merges14d` dimension off the network.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// One filesystem entry as this test compares them: entry kind, byte length,
/// mtime, and (for files) the exact contents. Contents are compared directly
/// rather than hashed — fixtures are tiny, and a mismatch then prints what
/// changed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    is_dir: bool,
    len: u64,
    mtime: Option<SystemTime>,
    contents: Option<Vec<u8>>,
}

/// Recursive snapshot of `root`, keyed by path relative to `root`. Directory
/// mtimes are included on purpose: a file created and deleted again within one
/// run still moves the mtime of the directory that briefly held it.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Entry> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Entry>) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let is_dir = meta.is_dir();
            let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
            out.insert(
                rel,
                Entry {
                    is_dir,
                    len: meta.len(),
                    mtime: meta.modified().ok(),
                    contents: if is_dir {
                        None
                    } else {
                        std::fs::read(&path).ok()
                    },
                },
            );
            if is_dir {
                walk(base, &path, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Report every path that differs between two snapshots, newest first in the
/// order a reader wants them: added, removed, then modified.
fn diff(before: &BTreeMap<PathBuf, Entry>, after: &BTreeMap<PathBuf, Entry>) -> Vec<String> {
    let mut out = Vec::new();
    for (path, entry) in after {
        match before.get(path) {
            None => out.push(format!("added   {}", path.display())),
            Some(prior) if prior != entry => {
                out.push(format!("changed {}", path.display()));
            }
            Some(_) => {}
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            out.push(format!("removed {}", path.display()));
        }
    }
    out
}

/// A fixture fleet: `n` workspace roots under a temp dir, a registry naming
/// them, and a fixture `HOME`. Nothing here is a git repo — `repo_slug` falls
/// back to the directory basename, which is all these tests read.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    registry: PathBuf,
    home: PathBuf,
    roots: Vec<PathBuf>,
}

fn fixture(n: usize) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let home = root.join("home");
    std::fs::create_dir_all(home.join(".loom")).expect("home/.loom");

    let mut roots = Vec::new();
    for i in 0..n {
        let ws = root.join("fleet").join(format!("repo-{i:02}"));
        std::fs::create_dir_all(&ws).expect("workspace root");
        // Alternate the `kind` dimension so stratification has something to do.
        let manifest = if i % 2 == 0 {
            "Cargo.toml"
        } else {
            "package.json"
        };
        std::fs::write(ws.join(manifest), b"{}\n").expect("manifest");
        roots.push(ws);
    }

    let registry = root.join("workspaces.json");
    let entries: Vec<serde_json::Value> = roots
        .iter()
        .map(|r| serde_json::json!({ "root": r, "priority": 100 }))
        .collect();
    std::fs::write(
        &registry,
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "workspaces": entries,
        }))
        .expect("registry json"),
    )
    .expect("write registry");

    Fixture {
        _dir: dir,
        root,
        registry,
        home,
        roots,
    }
}

/// Run `sweep-experiment plan` against the fixture, confined to it on every
/// axis of machine-level state the command could reach.
fn run_plan(fx: &Fixture, extra: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    cmd.args(["sweep-experiment", "plan", "--offline"])
        .args(extra)
        .current_dir(&fx.root)
        .env("HOME", &fx.home)
        .env("LOOM_WORKSPACES_PATH", &fx.registry)
        .env("LOOM_EXPERIMENTS_DIR", fx.home.join(".loom").join("experiments"));
    cmd.output().expect("run loom-daemon sweep-experiment plan")
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn assert_ok(out: &std::process::Output) {
    assert!(
        out.status.success(),
        "plan exited {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        stdout_of(out),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The headline property: a `plan` run with no `--out` leaves the filesystem
/// byte-for-byte as it found it — the workspace roots, the registry, and the
/// fixture `~/.loom` included.
#[test]
fn plan_without_out_writes_nothing_anywhere() {
    let fx = fixture(6);
    let before = snapshot(&fx.root);

    let out = run_plan(
        &fx,
        &[
            "--arms",
            "opus,sonnet",
            "--stratify",
            "merges14d,kind",
            "--seed",
            "7",
        ],
    );
    assert_ok(&out);

    let stdout = stdout_of(&out);
    for root in &fx.roots {
        let name = root.file_name().unwrap().to_string_lossy();
        assert!(
            stdout.contains(&*name),
            "every registered workspace must appear in the plan; {name} missing from:\n{stdout}"
        );
    }

    let after = snapshot(&fx.root);
    let changes = diff(&before, &after);
    assert!(
        changes.is_empty(),
        "`plan` must write nothing, but the fixture changed:\n  {}",
        changes.join("\n  ")
    );
    assert!(
        !fx.home.join(".loom").join("experiments").exists(),
        "`plan` must not create the experiments state dir — only `start` may"
    );
}

/// The writes-nothing assertion above is only worth anything if the harness
/// can see a write. `--out` is the one write `plan` is allowed to make, so it
/// doubles as the control: exactly one new file, and nothing else disturbed
/// beyond the directory that now holds it.
#[test]
fn the_writes_nothing_assertion_would_catch_a_write() {
    let fx = fixture(4);
    let before = snapshot(&fx.root);

    let plan_file = fx.root.join("plan.json");
    let out = run_plan(&fx, &["--seed", "3", "--out", plan_file.to_str().unwrap()]);
    assert_ok(&out);

    let after = snapshot(&fx.root);
    let added: Vec<_> = diff(&before, &after)
        .into_iter()
        .filter(|line| line.starts_with("added"))
        .collect();
    assert_eq!(
        added,
        vec!["added   plan.json".to_string()],
        "`--out` must write exactly one file (and the harness must see it)"
    );
    let doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&plan_file).expect("read plan")).expect("plan json");
    assert_eq!(doc["seed"], 3);
    assert_eq!(doc["workspaces"].as_array().map(Vec::len), Some(fx.roots.len()));
}

/// End-to-end determinism, at the process boundary the acceptance criterion
/// names: same seed ⇒ same assignment, different seed ⇒ a different one.
///
/// `created_at` is compared out (it is a wall-clock stamp, and two runs
/// straddling a second boundary would make an otherwise-sound assertion flaky),
/// and `experiment_id` is compared on its hash suffix only for the same reason
/// — its `YYYYMMDD` prefix would differ across a UTC midnight. The hash suffix
/// covers the seed, arms, strata and the full assignment, so comparing it is
/// the byte-identity claim that matters.
#[test]
fn plan_is_byte_identical_for_a_seed_and_differs_across_seeds() {
    let fx = fixture(8);

    let assignment = |seed: &str| -> serde_json::Value {
        let out = run_plan(
            &fx,
            &[
                "--json",
                "--arms",
                "opus,sonnet",
                "--stratify",
                "merges14d,kind",
                "--seed",
                seed,
            ],
        );
        assert_ok(&out);
        let mut doc: serde_json::Value =
            serde_json::from_str(&stdout_of(&out)).expect("plan --json emits a JSON document");
        // The acceptance criterion's "machine-readable enough for `start`":
        // these four fields plus the assignment must all be present.
        for key in ["seed", "arms", "stratify", "workspaces", "experiment_id"] {
            assert!(doc.get(key).is_some(), "plan document is missing {key}: {doc}");
        }
        let id = doc["experiment_id"]
            .as_str()
            .expect("experiment_id")
            .to_string();
        let hash = id.rsplit('-').next().expect("id hash suffix").to_string();
        doc["experiment_id"] = serde_json::Value::String(hash);
        doc.as_object_mut().expect("object").remove("created_at");
        doc
    };

    let first = assignment("7");
    let second = assignment("7");
    assert_eq!(
        serde_json::to_string_pretty(&first).unwrap(),
        serde_json::to_string_pretty(&second).unwrap(),
        "the same seed must produce a byte-identical plan"
    );

    let other_seed = assignment("8");
    let arms_of = |doc: &serde_json::Value| -> Vec<String> {
        doc["workspaces"]
            .as_array()
            .expect("workspaces")
            .iter()
            .map(|w| w["arm"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    assert_ne!(
        arms_of(&first),
        arms_of(&other_seed),
        "a different seed must produce a different assignment"
    );
}
