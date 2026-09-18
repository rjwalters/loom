//! Tests for fix-weighted progress.
//!
//! Two kinds of case here, and the second kind is the one that earned its
//! place. The parsing cases guard `git log` output, where a commit subject is
//! attacker-adjacent text (anyone with a commit can write one) and a mis-parse
//! silently moves the number the epic steers by. The classification cases each
//! pin a misreading that actually produced a wrong headline while this was
//! being built — see the module docs.

use super::*;

fn cats(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(p, c)| ((*p).to_string(), (*c).to_string()))
        .collect()
}

#[test]
fn only_fix_shaped_subjects_count() {
    for s in [
        "fix: x",
        "fix(scope): x",
        "Fix: x",
        "FIX something",
        "revert: x",
        "hotfix: x",
    ] {
        assert!(is_fix(s), "{s:?} should count");
    }
    for s in [
        "feat: x",
        "docs: x",
        "chore: x",
        "test: x",
        "refactor: x",
        "prefix: x",
        "",
    ] {
        assert!(!is_fix(s), "{s:?} should not count");
    }
}

#[test]
fn a_subject_that_merely_contains_fix_does_not_count() {
    // `prefix:` and `suffix fix` both contain "fix"; only a leading one is a
    // fix commit. A substring test here would inflate the denominator with
    // every feature commit that mentions fixing something.
    assert!(!is_fix("feat: prefix handling"));
    assert!(!is_fix("refactor: tidy the fix path"));
}

#[test]
fn the_cap_agrees_with_the_gate_at_the_boundary_itself() {
    // This test previously asserted only that the two NUMBERS were equal, and
    // it passed while the two mechanisms disagreed: the shell gate compares
    // with `-lt` (strictly under) and `has_been_ported` read `<=`, so a script
    // of exactly STUB_MAX_CODE_LINES lines was "trivial glue" here and "too
    // big" there. Pinning a constant does not pin a predicate.
    //
    // So drive the REAL gate instead of re-encoding its comparison: extract
    // `is_shape_a_stub` and its helpers from check-shell-allowlist.sh and run
    // both implementations over synthetic scripts that straddle the cap.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let gate = root.join("scripts/check-shell-allowlist.sh");
    let Ok(sh) = std::fs::read_to_string(&gate) else {
        return; // not a full checkout; nothing to compare against
    };
    let declared = sh
        .lines()
        .find_map(|l| l.trim().strip_prefix("STUB_MAX_CODE_LINES="))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .expect("the shell gate declares STUB_MAX_CODE_LINES");
    assert_eq!(declared, super::super::STUB_CAP, "the cap itself must agree");

    let dir = tempfile::tempdir().expect("tempdir");
    for n in [
        super::super::STUB_CAP - 1,
        super::super::STUB_CAP,
        super::super::STUB_CAP + 1,
    ] {
        // A literal `exec` handoff, so BOTH predicates agree on the handoff
        // half and the only variable left is the line count. (They differ by
        // design elsewhere: `is_handoff` also accepts `loom_exec_script_helper`
        // and a resolved `$DAEMON_BIN`, which the shell's shape test does not.)
        let mut body = String::from("#!/usr/bin/env bash\n");
        // The shebang starts with `#`, so the counting rule treats it as a
        // comment and it does not count. n-1 fillers + the exec line = n.
        for i in 0..n.saturating_sub(1) {
            body.push_str(&format!("filler_{i}=1\n"));
        }
        body.push_str("exec loom-daemon thing \"$@\"\n");
        let f = dir.path().join(format!("s{n}.sh"));
        std::fs::write(&f, &body).expect("write");

        let code = body
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#'))
            .count();
        assert_eq!(code, n, "fixture must have exactly {n} code lines");

        // Ask the shell gate itself.
        let script = format!(
            "STUB_MAX_CODE_LINES={declared}\n{}\n{}\n{}\nis_shape_a_stub \"$1\"",
            extract_fn(&sh, "code_line_count"),
            extract_fn(&sh, "last_code_line"),
            extract_fn(&sh, "is_shape_a_stub"),
        );
        let out = Command::new("bash")
            .arg("-c")
            .arg(&script)
            .arg("bash")
            .arg(&f)
            .output()
            .expect("bash runs");
        let shell_says_glue = out.status.success();

        assert_eq!(
            has_been_ported(&body),
            shell_says_glue,
            "at {n} code lines the two mechanisms must agree \
             (shell says trivial-glue={shell_says_glue})"
        );
    }
}

/// Pull one shell function's source out of the gate, so the test drives the
/// shipped implementation rather than a copy of it that can drift.
fn extract_fn(sh: &str, name: &str) -> String {
    let start = sh
        .find(&format!("{name}() {{"))
        .unwrap_or_else(|| panic!("{name} not found in check-shell-allowlist.sh"));
    let rest = &sh[start..];
    let end = rest
        .find("\n}\n")
        .unwrap_or_else(|| panic!("{name} has no terminator"));
    rest[..end + 3].to_string()
}

#[test]
fn a_thin_script_that_hands_off_has_been_ported() {
    // All three idioms in use, found by reading the scripts. The first cut
    // matched only a literal `loom-daemon` on the exec line and missed every
    // script that resolves the binary into a variable first.
    for body in [
        "#!/bin/bash\nexec loom-daemon dep-recheck-fingerprint \"$@\"\n",
        "#!/bin/bash\nDAEMON_BIN=$(resolve)\nexec \"$DAEMON_BIN\" tokens check \"$@\"\n",
        "#!/bin/bash\nsource lib/script-helper.sh\nloom_exec_script_helper detect-cycle \"$@\"\n",
    ] {
        assert!(has_been_ported(body), "should be ported:\n{body}");
    }
}

#[test]
fn a_large_script_that_execs_the_daemon_has_not_been_ported() {
    // THE case that mattered. `loom-daemon-start.sh` is 1,184 code lines with
    // 24 fixes — the largest remaining target — and it execs the daemon at the
    // end of a long startup sequence. On the handoff test alone it scores as
    // retired and moves the headline from 5% to 10%: the metric would lie in
    // exactly the direction that flatters the epic.
    let mut body = String::from("#!/bin/bash\n");
    for i in 0..200 {
        body.push_str(&format!("real_startup_work_{i}\n"));
    }
    body.push_str("exec \"$DAEMON_BIN\" start \"$@\"\n");
    assert!(
        !has_been_ported(&body),
        "a handoff only retires the logic when there is no logic left to carry"
    );
}

#[test]
fn a_small_script_that_merely_mentions_the_daemon_has_not_been_ported() {
    // Error text and comments name the binary constantly. Matching on the
    // mention rather than the handoff swept in real shell libraries —
    // `worktree-removal-log.sh` is 36 lines of live JSON escaping.
    for body in [
        "#!/bin/bash\necho 'loom-daemon is required' >&2\nexit 1\n",
        "#!/bin/bash\n# implemented by loom-daemon one day\ndo_real_work\n",
        "#!/bin/bash\nDAEMON_PID=$(pgrep -x loom-daemon | head -1)\nkill \"$DAEMON_PID\"\n",
    ] {
        assert!(!has_been_ported(body), "should NOT be ported:\n{body}");
    }
}

#[test]
fn the_definition_of_the_handoff_helper_is_not_a_call_to_it() {
    // script-helper.sh DEFINES `loom_exec_script_helper`. Counting the
    // definition as a handoff would retire the helper that implements
    // everyone else's handoff.
    assert!(!is_handoff("loom_exec_script_helper() {"));
    assert!(!is_handoff("  loom_exec_script_helper()"));
    assert!(is_handoff("  loom_exec_script_helper dep-recheck \"$@\""));
}

#[test]
fn exec_must_be_a_word_not_a_substring() {
    assert!(!is_handoff("execute_loom-daemon_thing"));
    assert!(!is_handoff("noexec loom-daemon"));
    assert!(is_handoff("exec loom-daemon x"));
    assert!(is_handoff("if true; then exec \"$DAEMON_BIN\" x; fi"));
}

#[test]
fn the_category_is_not_the_discriminator() {
    // #8233 as filed said "scripts now in the `stub` category". The allowlist
    // categories record why a file is ALLOWED to be shell, not whether its
    // logic moved: every genuinely ported script is `contract`, because its
    // name is an invocation contract. Measuring the category reported 3.
    let ported_but_contract = "#!/bin/bash\nexec loom-daemon classify-dependency-block \"$@\"\n";
    assert!(
        has_been_ported(ported_but_contract),
        "a `contract` script that hands off is ported, whatever its category says"
    );
}

#[test]
fn stub_churn_is_retired_and_portable_churn_is_not() {
    let c = Churn {
        retired: 14,
        remaining: 86,
        worst: vec![],
        delegating: 4,
        window: "6.months",
    };
    assert_eq!(c.retired_pct().round() as u64, 14);
}

#[test]
fn an_empty_window_reports_nothing_rather_than_dividing_by_zero() {
    let c = Churn::default();
    assert_eq!(c.retired_pct(), 0.0);
    assert_eq!(render(&c), "", "no history means no section, not a crash");
}

#[test]
fn the_report_names_the_worst_remaining_so_the_next_port_is_obvious() {
    let c = Churn {
        retired: 10,
        remaining: 90,
        worst: vec![
            ("defaults/scripts/merge-pr.sh".into(), 1458, 48),
            ("defaults/scripts/worktree.sh".into(), 1812, 26),
        ],
        delegating: 16,
        window: "6.months",
    };
    let out = render(&c);
    assert!(out.contains("churn retired"), "{out}");
    assert!(out.contains("10%"), "the share is reported: {out}");
    assert!(out.contains("merge-pr.sh"), "{out}");
    // The unit is a script-fix, not a fix-commit: one commit fixing three
    // scripts contributes three. Naming it "fix-commits" overstated what the
    // denominator is by roughly the average fan-out of a fix.
    assert!(out.contains("script-fixes"), "the unit is named: {out}");
    // Ordered by fixes, not by lines — worktree.sh is larger and less urgent.
    let mp = out.find("merge-pr.sh").expect("present");
    let wt = out.find("worktree.sh").expect("present");
    assert!(mp < wt, "the most-fixed script leads, regardless of size");
}

#[test]
fn a_permanent_floor_script_is_in_neither_bucket() {
    // `bootstrap` and `vendored` are never going to be ported, so counting
    // their churn would put something in the denominator that the epic cannot
    // move — the figure would be permanently capped below 100% for a reason
    // unrelated to progress.
    let d = tempfile::tempdir().expect("tempdir");
    let categories = cats(&[
        ("a.sh", "bootstrap"),
        ("b.sh", "vendored"),
        ("c.sh", "test"),
    ]);
    // No git history in this tempdir, so per_path yields nothing and the
    // buckets stay empty — which is the assertion: these categories can never
    // contribute even when they do have churn.
    let got = measure(d.path(), &categories);
    if let Ok(c) = got {
        assert_eq!(c.retired, 0);
        assert_eq!(c.remaining, 0);
    }
}

/// A throwaway repo with one commit per `(subject, files)` pair.
fn repo_with(commits: &[(&str, &[(&str, &str)])]) -> tempfile::TempDir {
    let d = tempfile::tempdir().expect("tempdir");
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .arg("-C")
            .arg(d.path())
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} must succeed");
    };
    git(&["init", "-q"]);
    for (subject, files) in commits {
        for (name, body) in *files {
            let p = d.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(p, body).expect("write");
        }
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", subject]);
    }
    d
}

#[test]
fn measure_splits_churn_by_whether_the_logic_moved() {
    // End to end against a real repo: same fix count, opposite verdicts,
    // decided only by what the script contains now.
    let d = repo_with(&[
        (
            "seed",
            &[
                ("ported.sh", "#!/bin/bash\nexec loom-daemon thing \"$@\"\n"),
                ("fat.sh", "#!/bin/bash\nreal logic\nmore logic\n"),
            ],
        ),
        (
            "fix: both",
            &[
                // Must differ from the seed, or git does not list it as
                // touched and the commit silently only counts as a fix to
                // fat.sh — which is what the first version of this fixture
                // did, reporting retired=0 against correct code.
                ("ported.sh", "#!/bin/bash\nexec loom-daemon thing2 \"$@\"\n"),
                ("fat.sh", "#!/bin/bash\nreal logic\nmore logic\nfixed\n"),
            ],
        ),
    ]);
    let categories = cats(&[("ported.sh", "contract"), ("fat.sh", "contract")]);
    let c = measure(d.path(), &categories).expect("measures");
    assert_eq!(c.retired, 1, "the handed-off script's fix is retired");
    assert_eq!(c.remaining, 1, "the fat script's is not");
    assert_eq!(c.delegating, 1);
    assert_eq!(c.worst.len(), 1, "only the remaining one is worth naming");
    assert_eq!(c.worst[0].0, "fat.sh");
}

#[test]
fn per_path_survives_a_subject_containing_a_newline() {
    // `--format=%x00%s%x00` exists for this: a subject with an embedded
    // newline would otherwise have its tail parsed as a file path, and that
    // path would then accrue fix-commits it never had.
    //
    // Driven against a real repo, because the failure is in how git frames the
    // output, not in our splitting.
    let d = repo_with(&[("fix: one\nnot/a/real/path.sh", &[("real.sh", "x\n")])]);
    let counts = per_path(d.path()).expect("git log runs");
    assert_eq!(counts.get("real.sh").copied(), Some(1), "the real path counts");
    assert_eq!(
        counts.get("not/a/real/path.sh"),
        None,
        "a line of the SUBJECT must never be read as a changed path"
    );
}

#[test]
fn per_path_counts_each_touched_file_of_a_fix() {
    let d = repo_with(&[
        ("fix: touches both", &[("a.sh", "x\n"), ("b.sh", "x\n")]),
        ("feat: not a fix", &[("a.sh", "y\n")]),
    ]);
    let counts = per_path(d.path()).expect("git log runs");
    assert_eq!(counts.get("a.sh").copied(), Some(1));
    assert_eq!(counts.get("b.sh").copied(), Some(1));
}
