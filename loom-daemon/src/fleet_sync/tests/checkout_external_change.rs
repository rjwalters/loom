//! Tests for out-of-band checkout changes (#11566). Real git in temp dirs; the
//! 2am reflog is a faithful synthetic fixture (see `FIXTURE`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use tempfile::TempDir;

use super::super::{watch_reflog, CheckoutPass, Env, Memory, MoveHold};
use super::*;
use crate::workspace_hold::{HeldCopy, HoldKind, WorkspaceHold};

/// A synthetic stand-in for the reflog of the checkout reset at 00:44:39,
/// 00:44:57 and 00:45:16 on 2026-10-12. The real reflog was not available to
/// the builder, so the lines are faithful in shape (git's on-disk format, the
/// three reset times, unpushed commits before them, Loom's own tagged
/// fast-forwards around them, and a no-op `reset: moving to HEAD`) but the
/// shas and messages are invented.
const FIXTURE: &str = include_str!("../../../tests/fixtures/reflog-2am-2026-10-12.txt");

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_REFLOG_ACTION")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit(dir: &Path, name: &str) {
    fs::write(dir.join(name), name).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--quiet", "-m", &format!("add {name}")]);
}

/// A repo on `main` with one commit.
fn repo() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let dir = fs::canonicalize(tmp.path()).unwrap().join("host");
    fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "--quiet", "--initial-branch=main"]);
    for (k, v) in [("user.name", "T"), ("user.email", "t@example.invalid")] {
        git(&dir, &["config", k, v]);
    }
    git(&dir, &["config", "commit.gpgsign", "false"]);
    commit(&dir, "first");
    (tmp, dir)
}

fn watch(root: &Path, memory: &mut Memory) -> CheckoutPass {
    let mut pass = CheckoutPass::default();
    watch_reflog(root, Some("main"), Utc::now(), memory, &mut pass);
    pass
}

// ---------------------------------------------------------------- parsing

#[test]
fn the_2am_fixture_yields_the_three_resets() {
    let entries: Vec<Entry> = FIXTURE.lines().filter_map(parse_line).collect();
    assert_eq!(entries.len(), FIXTURE.lines().count(), "every line parses");
    let kinds: Vec<_> = entries
        .iter()
        .filter_map(|e| classify(e).map(|k| (e.at, k)))
        .collect();
    let at = |h, m, s| Utc.with_ymd_and_hms(2026, 10, 12, h, m, s).unwrap();
    assert_eq!(
        kinds,
        vec![
            (at(0, 44, 39), Kind::Reset),
            (at(0, 44, 57), Kind::Reset),
            (at(0, 45, 16), Kind::Reset),
        ],
        "Loom's tagged ff, plain commits and the no-op reset are not external changes"
    );
}

#[test]
fn replaying_the_fixture_past_a_seeded_mark_finds_each_reset_once() {
    let head = String::from("HEAD");
    let lines: Vec<&str> = FIXTURE.lines().collect();
    let seeded = lines[..4].join("\n");
    let mut marks = Marks::default();
    assert!(marks.scan(&[(head.clone(), seeded)]).found.is_empty(), "first sight seeds");
    let scan = marks.scan(&[(head.clone(), FIXTURE.to_string())]);
    let times: Vec<_> = scan
        .found
        .iter()
        .map(|(_, e, _)| e.at.format("%H:%M:%S").to_string())
        .collect();
    assert_eq!(times, ["00:44:39", "00:44:57", "00:45:16"]);
    assert!(marks.scan(&[(head, FIXTURE.to_string())]).found.is_empty(), "second look: none");
}

#[test]
fn a_reset_in_both_logs_is_one_entry() {
    let line = "0000000000000000000000000000000000000002 0000000000000000000000000000000000000001 \
                T <t@x> 1791765879 +0000\treset: moving to HEAD~1\n";
    let old = "0000000000000000000000000000000000000001 0000000000000000000000000000000000000002 \
               T <t@x> 1791765800 +0000\tcommit: x\n";
    let mut marks = Marks::default();
    let logs = |text: String| {
        vec![
            ("HEAD".to_string(), text.clone()),
            ("refs/heads/main".into(), text),
        ]
    };
    marks.scan(&logs(old.to_string()));
    assert_eq!(marks.scan(&logs(format!("{old}{line}"))).found.len(), 1);
}

#[test]
fn a_shrunk_or_rewritten_log_is_unknown_not_a_change() {
    let l = |n: u8, msg: &str| {
        format!("{:040x} {:040x} T <t@x> 17917658{n:02} +0000\t{msg}\n", n, n + 1)
    };
    let mut marks = Marks::default();
    let log = format!("{}{}", l(1, "commit: a"), l(2, "commit: b"));
    marks.scan(&[("HEAD".into(), log)]);
    let rewritten = format!("{}{}", l(1, "commit: a"), l(9, "reset: moving to x"));
    assert!(marks.scan(&[("HEAD".into(), rewritten)]).found.is_empty());
    assert!(marks
        .scan(&[("HEAD".into(), l(3, "reset: moving to y"))])
        .found
        .is_empty());
}

// ---------------------------------------------------------------- real git

#[test]
fn first_sight_seeds_silently() {
    let (_t, dir) = repo();
    commit(&dir, "second");
    let mut memory = Memory::default();
    assert!(watch(&dir, &mut memory).external_changes.is_empty());
}

#[test]
fn an_external_reset_emits_exactly_one_fact_with_the_discarded_commits() {
    let (_t, dir) = repo();
    let mut memory = Memory::default();
    watch(&dir, &mut memory);
    commit(&dir, "unpushed-a");
    commit(&dir, "unpushed-b");
    let tip = git(&dir, &["rev-parse", "HEAD"]);
    let first = git(&dir, &["rev-parse", "HEAD~2"]);
    assert!(watch(&dir, &mut memory).external_changes.is_empty(), "commits are additive");

    git(&dir, &["reset", "--quiet", "--hard", "HEAD~2"]);
    let pass = watch(&dir, &mut memory);
    assert_eq!(pass.external_changes.len(), 1, "{:?}", pass.external_changes);
    let fact = &pass.external_changes[0];
    assert_eq!(
        (fact.move_kind.as_str(), fact.from_sha.as_str(), fact.to_sha.as_str()),
        ("reset", tip.as_str(), first.as_str())
    );
    assert!(fact.reflog_line.starts_with("reset:"), "{}", fact.reflog_line);
    assert_eq!(fact.root, dir.display().to_string());
    assert!(!fact.host.is_empty());
    assert!(!fact.hold_active);
    let discarded = fact.discarded_commits.as_ref().unwrap();
    assert_eq!(discarded.count, 2);
    assert_eq!(discarded.subjects, ["add unpushed-b", "add unpushed-a"]);

    assert!(watch(&dir, &mut memory).external_changes.is_empty(), "seen once, never again");
}

#[test]
fn a_commit_still_on_another_branch_is_not_discarded() {
    let (_t, dir) = repo();
    let mut memory = Memory::default();
    watch(&dir, &mut memory);
    commit(&dir, "kept");
    git(&dir, &["branch", "rescue"]);
    git(&dir, &["reset", "--quiet", "--hard", "HEAD~1"]);
    let pass = watch(&dir, &mut memory);
    assert_eq!(
        pass.external_changes[0]
            .discarded_commits
            .as_ref()
            .unwrap()
            .count,
        0
    );
}

#[test]
fn an_external_checkout_is_reported() {
    let (_t, dir) = repo();
    let mut memory = Memory::default();
    watch(&dir, &mut memory);
    git(&dir, &["checkout", "--quiet", "-b", "elsewhere"]);
    commit(&dir, "x");
    git(&dir, &["checkout", "--quiet", "main"]);
    let pass = watch(&dir, &mut memory);
    let kinds: Vec<_> = pass
        .external_changes
        .iter()
        .map(|f| f.move_kind.as_str())
        .collect();
    // Creating `elsewhere` at the same commit moved nothing; leaving it did.
    assert_eq!(kinds, ["checkout"], "{:?}", pass.external_changes);
}

/// A bare origin and a clone of it, plus a second clone that pushes.
fn with_origin() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let origin = base.join("origin.git");
    fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "--quiet", "--bare", "--initial-branch=main"]);
    let clone = |name: &str| {
        git(&base, &["clone", "--quiet", origin.to_str().unwrap(), name]);
        let dir = base.join(name);
        for (k, v) in [("user.name", "T"), ("user.email", "t@example.invalid")] {
            git(&dir, &["config", k, v]);
        }
        git(&dir, &["config", "commit.gpgsign", "false"]);
        dir
    };
    let seed = clone("seed");
    commit(&seed, "first");
    git(&seed, &["push", "--quiet", "origin", "HEAD:main"]);
    let host = clone("host");
    commit(&seed, "second");
    git(&seed, &["push", "--quiet", "origin", "HEAD:main"]);
    (tmp, host, seed)
}

#[test]
fn loom_own_fast_forward_emits_nothing_and_logs_the_loom_tag() {
    let (_t, host, _seed) = with_origin();
    let mut memory = Memory::default();
    watch(&host, &mut memory);
    git(&host, &["fetch", "--quiet", "origin"]);
    super::super::git::fast_forward(&host, "main").unwrap();
    let log = fs::read_to_string(host.join(".git/logs/HEAD")).unwrap();
    assert!(log.lines().last().unwrap().contains("\tloom:ff:"), "{log}");
    assert!(watch(&host, &mut memory).external_changes.is_empty());
    // The same checkout, reset by hand afterwards, is reported.
    git(&host, &["reset", "--quiet", "--hard", "HEAD~1"]);
    assert_eq!(watch(&host, &mut memory).external_changes.len(), 1);
}

#[test]
fn a_standing_hold_is_recorded_and_cited_when_it_clears() {
    let (_t, dir) = repo();
    let mut memory = Memory::default();
    watch(&dir, &mut memory);
    let began = Utc::now() - chrono::Duration::minutes(5);
    let hold = WorkspaceHold {
        kind: HoldKind::InstallIncompatible,
        copy: HeldCopy::Checkout,
        since: began,
        detail: "test".into(),
        verdict_at: began,
    };
    crate::workspace_hold::set_for_test(&dir, Some(hold));
    commit(&dir, "unpushed");
    watch(&dir, &mut memory);
    git(&dir, &["reset", "--quiet", "--hard", "HEAD~1"]);
    let pass = watch(&dir, &mut memory);
    crate::workspace_hold::set_for_test(&dir, None);
    let fact = &pass.external_changes[0];
    assert!(fact.hold_active);
    let note = cited(&dir, began);
    assert!(note.contains(&fact.change_id), "{note}");
    assert_eq!(
        cited(&dir, Utc::now() + chrono::Duration::minutes(1)),
        "",
        "later holds are not cited"
    );
}

#[test]
fn a_pass_over_the_root_reports_the_change() {
    let (_t, dir) = repo();
    let mut memory = Memory::default();
    let roots = vec![dir.clone()];
    let run_pass = |memory: &mut Memory| {
        let env = Env {
            write: false,
            held: &|| None,
            gate_in_flight: &|_| false,
            hold: &|_| Some(MoveHold::free()),
            network: false,
            backing_off: &|_| false,
            confirmed: &|_, _| None,
            breaker_open: &|| false,
            budget: None,
            elapsed: &|| Duration::ZERO,
            clock: &Utc::now,
        };
        super::super::run(&env, &roots, memory)
    };
    run_pass(&mut memory);
    commit(&dir, "unpushed");
    git(&dir, &["reset", "--quiet", "--hard", "HEAD~1"]);
    let pass = run_pass(&mut memory);
    assert_eq!(pass.external_changes.len(), 1, "{:?}", pass.external_changes);
    assert!(run_pass(&mut memory).external_changes.is_empty());
}

#[test]
fn the_record_round_trips_and_names_the_ref() {
    let (_t, dir) = repo();
    let mut memory = Memory::default();
    watch(&dir, &mut memory);
    commit(&dir, "x");
    git(&dir, &["reset", "--quiet", "--hard", "HEAD~1"]);
    let fact = watch(&dir, &mut memory).external_changes.remove(0);
    let json = serde_json::to_value(&fact).unwrap();
    assert!(json["ref"] == "HEAD" || json["ref"] == "refs/heads/main");
    let back: CheckoutExternalChangeRecord = serde_json::from_value(json).unwrap();
    assert_eq!(back, fact);
}
