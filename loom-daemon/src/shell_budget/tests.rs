//! Tests for `shell_budget.rs` — the ratchet decision, the allowlist
//! invariants, and the #8154 declaration rules.
//!
//! Split out of the parent so the over-threshold file shrinks rather than
//! grows (`.loom/docs/file-size-policy.md`, #7718).

use super::*;

fn budget_of(pairs: &[(&str, u64)]) -> Budget {
    let mut b = Budget::default();
    for (c, n) in pairs {
        b.by_category.insert((*c).to_string(), *n);
        b.files_by_category.insert((*c).to_string(), 1);
    }
    b
}

fn budget_with(pairs: &[(&str, u64, u64)]) -> Budget {
    let mut b = Budget::default();
    for (c, lines, files) in pairs {
        b.by_category.insert((*c).to_string(), *lines);
        b.files_by_category.insert((*c).to_string(), *files);
    }
    b
}

#[test]
fn adding_portable_shell_fails_and_names_the_category() {
    let before = budget_with(&[("contract", 100, 2), ("bootstrap", 50, 1)]);
    let now = budget_with(&[("contract", 130, 3), ("bootstrap", 50, 1)]);
    let err = check_against_rev(&now, &before, "origin/main (abc)", &GrowthContext::none())
        .expect_err("must fail");
    assert!(err.contains("adds 30 code lines of PORTABLE"), "{err}");
    assert!(err.contains("contract     100 -> 130"), "{err}");
    assert!(err.contains("MERGE-BASE"), "must say what it compared against: {err}");
}

#[test]
fn growth_in_the_permanent_floor_is_caught_too() {
    // Regression: the first cut of the merge-base redesign dropped the
    // total check entirely, so a 500-line `bootstrap` script passed where
    // it used to fail. Portable is what the epic retires, but floor growth
    // raises the finish line and must be deliberate.
    let before = budget_with(&[("contract", 100, 2), ("bootstrap", 50, 1)]);
    let now = budget_with(&[("contract", 100, 2), ("bootstrap", 550, 2)]);
    let err = check_against_rev(&now, &before, "origin/main (abc)", &GrowthContext::none())
        .expect_err("must fail");
    assert!(err.contains("adds 500 code lines of production shell"), "{err}");
    assert!(err.contains("permanent floor"), "{err}");
}

#[test]
fn recategorising_to_hide_an_addition_is_caught() {
    // Regression: moving a 639-line file `contract` -> `bootstrap` while
    // ADDING 200 portable lines reported -439 and passed. Portable falls,
    // but total rises, so the total leg catches it.
    let before = budget_with(&[("contract", 1000, 10), ("bootstrap", 50, 1)]);
    let now = budget_with(&[("contract", 561, 9), ("bootstrap", 889, 2)]);
    assert!(now.portable() < before.portable(), "portable falls, as in the report");
    let err = check_against_rev(&now, &before, "origin/main (abc)", &GrowthContext::none())
        .expect_err("must fail");
    assert!(err.contains("production shell"), "{err}");
}

#[test]
fn a_change_that_removes_shell_passes() {
    let before = budget_with(&[("contract", 100, 2), ("bootstrap", 50, 1)]);
    let now = budget_with(&[("contract", 40, 1), ("bootstrap", 50, 1)]);
    assert!(check_against_rev(&now, &before, "base", &GrowthContext::none()).is_ok());
}

#[test]
fn a_change_that_moves_shell_sideways_passes() {
    // Add and remove an equal amount: net zero, allowed by design — it is
    // option 2 in the failure message.
    let before = budget_with(&[("contract", 100, 2), ("hook-entry", 20, 1)]);
    let now = budget_with(&[("contract", 80, 2), ("hook-entry", 40, 2)]);
    assert_eq!(now.portable(), before.portable());
    assert!(check_against_rev(&now, &before, "base", &GrowthContext::none()).is_ok());
}

#[test]
fn an_unchanged_tree_passes() {
    let b = budget_with(&[("contract", 100, 2)]);
    assert!(check_against_rev(&b, &b, "base", &GrowthContext::none()).is_ok());
}

// --- #8154: declared floor growth ---

/// A realistic commit message, because position is now part of the rule:
/// a subject line, a blank, then the declaration.
fn decl(lines: u64) -> Vec<GrowthDeclaration> {
    parse_growth_declarations(&format!(
        "fix: add a guard\n\nShell-Budget-Growth: {lines} lines — must stay shell, see (#7870)"
    ))
    .0
}

/// Wrap a body so its first line is a subject, not a declaration.
fn msg(body: &str) -> String {
    format!("subject line\n\n{body}")
}

#[test]
fn a_declaration_covering_the_growth_admits_floor_growth() {
    // The case #8154 was filed for: #7870 adds 59 lines to a `vendored`
    // script it cannot port (blocked upstream by #7758), and the only
    // alternative the gate offered was deleting the guard it was adding.
    let before = budget_with(&[("contract", 100, 2), ("vendored", 500, 1)]);
    let now = budget_with(&[("contract", 100, 2), ("vendored", 559, 1)]);
    assert!(check_against_rev(
        &now,
        &before,
        "base",
        &GrowthContext {
            declared: &decl(59)
        }
    )
    .is_ok());
}

#[test]
fn a_declaration_larger_than_the_growth_also_admits_it() {
    // Declaring a ceiling and coming in under it is honest, not a defect.
    let before = budget_with(&[("bootstrap", 500, 1)]);
    let now = budget_with(&[("bootstrap", 520, 1)]);
    assert!(check_against_rev(
        &now,
        &before,
        "base",
        &GrowthContext {
            declared: &decl(59)
        }
    )
    .is_ok());
}

#[test]
fn a_declaration_short_of_the_growth_still_fails_and_says_by_how_much() {
    // The override is a declared amount, not a blanket pass. Declaring 10
    // and growing 500 must not buy the other 490.
    let before = budget_with(&[("bootstrap", 50, 1)]);
    let now = budget_with(&[("bootstrap", 550, 2)]);
    let err = check_against_rev(
        &now,
        &before,
        "base",
        &GrowthContext {
            declared: &decl(10),
        },
    )
    .expect_err("must fail");
    assert!(err.contains("You declared 10 line(s)"), "{err}");
    assert!(err.contains("490 short"), "must name the shortfall: {err}");
}

#[test]
fn undeclared_growth_remains_default_deny() {
    let before = budget_with(&[("bootstrap", 50, 1)]);
    let now = budget_with(&[("bootstrap", 550, 2)]);
    assert!(check_against_rev(&now, &before, "base", &GrowthContext::none()).is_err());
}

#[test]
fn a_declaration_never_admits_portable_growth() {
    // The trailer buys a bigger permanent floor. It must not buy more of
    // the thing the epic exists to retire, or the gate is decorative.
    let before = budget_with(&[("contract", 100, 2)]);
    let now = budget_with(&[("contract", 130, 3)]);
    // Without this the test would pass vacuously if `decl` ever returned
    // an empty vec — it would then be asserting the default-deny path.
    assert_eq!(decl(9999).len(), 1, "the fixture must actually declare");
    let err = check_against_rev(
        &now,
        &before,
        "base",
        &GrowthContext {
            declared: &decl(9999),
        },
    )
    .expect_err("must fail");
    assert!(err.contains("PORTABLE"), "{err}");
}

#[test]
fn the_failure_message_describes_the_format_the_code_accepts() {
    // The whole bug in #8154 was a message promising something the code did
    // not implement. Pin them together: the shape the message prints must
    // parse, and must then admit the growth it was printed for.
    let before = budget_with(&[("bootstrap", 50, 1)]);
    let now = budget_with(&[("bootstrap", 550, 2)]);
    let err =
        check_against_rev(&now, &before, "base", &GrowthContext::none()).expect_err("must fail");

    assert!(err.contains(GROWTH_TRAILER), "message must name the trailer: {err}");

    // Lift the literal template out of the message and make it real.
    let line = err
        .lines()
        .find(|l| l.contains(GROWTH_TRAILER))
        .expect("message must show the trailer line");
    let concrete = line
        .replace("<why this must stay shell>", "cannot be ported yet")
        .replace("<issue>", "8154");
    let (ok, bad) = parse_growth_declarations(&msg(&concrete));
    assert!(bad.is_empty(), "the message's own template must parse: {bad:?}");
    assert_eq!(ok.len(), 1, "from {concrete:?}");
    assert_eq!(ok[0].lines, 500, "must carry the measured growth");
    assert!(check_against_rev(&now, &before, "base", &GrowthContext { declared: &ok }).is_ok());
}

#[test]
fn declarations_accumulate_across_commits_in_the_range() {
    let (ok, bad) = parse_growth_declarations(
            "feat: one\n\nShell-Budget-Growth: 30 lines — first half (#8154)\n\n             feat: two\n\nShell-Budget-Growth: 29 lines — second half (#8154)\n",
        );
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(ok.len(), 2);
    let before = budget_with(&[("vendored", 500, 1)]);
    let now = budget_with(&[("vendored", 559, 1)]);
    assert!(check_against_rev(&now, &before, "base", &GrowthContext { declared: &ok }).is_ok());
}

#[test]
fn a_declaration_without_an_issue_is_malformed_not_accepted() {
    // Ask #1: the reason must reference an issue, so the override cannot be
    // a bare escape hatch a Builder grants itself in passing.
    let (ok, bad) =
        parse_growth_declarations(&msg("Shell-Budget-Growth: 59 lines — because I said so"));
    assert!(ok.is_empty(), "{ok:?}");
    assert_eq!(bad.len(), 1);
    assert!(bad[0].why.contains("cites no issue"), "{:?}", bad[0]);
}

#[test]
fn a_declaration_without_a_count_or_reason_is_malformed() {
    let (ok, bad) = parse_growth_declarations(&msg(
        "Shell-Budget-Growth: lines — no count (#1)\nShell-Budget-Growth: 12 lines\n",
    ));
    assert!(ok.is_empty(), "{ok:?}");
    assert_eq!(bad.len(), 2, "{bad:?}");
    assert!(bad[0].why.contains("no leading line count"), "{:?}", bad[0]);
    assert!(bad[1].why.contains("no reason"), "{:?}", bad[1]);
}

#[test]
fn the_trailer_parses_across_plausible_separators_and_casing() {
    // Authors type what the message shows, but not byte-for-byte. An
    // em-dash, a hyphen, a colon and a lowercase key all mean the same
    // thing, and a near-miss that silently means "no override" is the
    // failure mode this whole issue is about.
    for body in [
        "Shell-Budget-Growth: 59 lines — why (#7870)",
        "Shell-Budget-Growth: 59 lines - why (#7870)",
        "Shell-Budget-Growth: 59 lines: why (#7870)",
        "Shell-Budget-Growth: 59 why (#7870)",
        "shell-budget-growth: 59 lines — why (#7870)",
        "Shell-Budget-Growth: 1 line — why (#7870)",
    ] {
        let (ok, bad) = parse_growth_declarations(&msg(body));
        assert!(bad.is_empty(), "{body:?} -> {bad:?}");
        assert_eq!(ok.len(), 1, "{body:?}");
        assert_eq!(ok[0].issue, 7870, "{body:?}");
    }
}

#[test]
fn an_indented_trailer_is_prose_not_a_declaration() {
    // Review found this on the very PR that added the parser: the commit
    // message contained an INDENTED example of the trailer, the parser
    // trimmed before matching, and the PR granted itself 59 lines
    // attributed to an unmerged issue. A squash-merge would have written
    // that into `main`'s cumulative figure permanently.
    let body = "fix(shell-budget): make the message real\n\n\
                    If the growth is right, declare it like this:\n\n\
                    \x20   Shell-Budget-Growth: 59 lines — an example (#7870)\n\n\
                    That is all.\n";
    let (ok, bad) = parse_growth_declarations(body);
    assert!(ok.is_empty(), "an indented example must not declare: {ok:?}");
    // It IS reported. Silently ignoring a near-miss is how the author ends
    // up reading a message about growth when the real problem is
    // placement — the defect this change exists to fix.
    assert_eq!(bad.len(), 1, "a near-miss must be reported: {bad:?}");
    assert!(bad[0].why.contains("indented"), "{:?}", bad[0]);

    // The same text at column 0 IS a declaration — otherwise this test
    // would pass simply because the parser stopped working.
    let real = msg("Shell-Budget-Growth: 59 lines — an example (#7870)\n");
    assert_eq!(parse_growth_declarations(&real).0.len(), 1);
}

#[test]
fn a_fenced_declaration_is_an_example_and_is_reported() {
    // Review got a column-0 trailer past the first cut by putting it in a
    // fenced block, which is how commit bodies in this repo quote things.
    let body = msg("Here is the format:\n\n```\nShell-Budget-Growth: 5 lines — x (#1)\n```\n");
    let (ok, bad) = parse_growth_declarations(&body);
    assert!(ok.is_empty(), "{ok:?}");
    assert_eq!(bad.len(), 1);
    assert!(bad[0].why.contains("fenced"), "{:?}", bad[0]);
}

#[test]
fn an_alternating_fence_marker_does_not_unfence_an_example() {
    // ``` does not close a ~~~ fence. Treating any marker as a toggle let
    // an alternating pair leave the block and admit a quoted example.
    let body = msg("```\n~~~\nShell-Budget-Growth: 5 lines — x (#1)\n```\n");
    let (ok, bad) = parse_growth_declarations(&body);
    assert!(ok.is_empty(), "still inside the ``` fence: {ok:?}");
    assert_eq!(bad.len(), 1);
    assert!(bad[0].why.contains("fenced"), "{:?}", bad[0]);
}

#[test]
fn an_indented_fence_marker_is_literal_code_not_a_fence() {
    // 4+ spaces makes ``` literal in Markdown. Toggling on it silently
    // refused the real declaration that followed.
    let body = msg("    ```\n\nShell-Budget-Growth: 5 lines — x (#1)\n");
    let (ok, bad) = parse_growth_declarations(&body);
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(ok.len(), 1, "{ok:?}");
}

#[test]
fn a_leading_blank_line_does_not_shift_the_subject() {
    // `--cleanup=verbatim` keeps a leading blank. git still calls the
    // first non-blank line the subject; indexing on line 0 did not.
    let (ok, bad) = parse_growth_declarations("\nShell-Budget-Growth: 5 lines — x (#1)\n");
    assert!(ok.is_empty(), "{ok:?}");
    assert_eq!(bad.len(), 1);
    assert!(bad[0].why.contains("SUBJECT"), "{:?}", bad[0]);
}

#[test]
fn folding_does_not_absorb_a_following_field() {
    // Folding swallowed an indented `Closes #1` into a reason that cited
    // no issue — manufacturing the very citation the rule requires — and
    // pulled `Co-Authored-By:` in with it.
    let body = msg(
        "Shell-Budget-Growth: 5 lines — no issue here\n  Closes #1\n  Co-Authored-By: X <a@b>\n",
    );
    let (ok, bad) = parse_growth_declarations(&body);
    assert!(ok.is_empty(), "the citation must not be manufactured: {ok:?}");
    assert_eq!(bad.len(), 1);
    assert!(bad[0].why.contains("cites no issue"), "{:?}", bad[0]);

    // A genuine wrapped reason still folds.
    let good = msg("Shell-Budget-Growth: 5 lines — a reason that\n  wraps here (#1)\n");
    assert_eq!(parse_growth_declarations(&good).0.len(), 1);
}

#[test]
fn a_declaration_as_the_subject_is_rejected_and_reported() {
    // git would never treat a subject as a trailer, and a squash rewrites
    // it to `* Shell-Budget-Growth: …`, where it would stop counting —
    // green on the PR, red on main.
    let (ok, bad) = parse_growth_declarations("Shell-Budget-Growth: 5 lines — x (#1)\n\nbody");
    assert!(ok.is_empty(), "{ok:?}");
    assert_eq!(bad.len(), 1);
    assert!(bad[0].why.contains("SUBJECT"), "{:?}", bad[0]);
}

#[test]
fn a_declaration_survives_the_repos_own_commit_shape() {
    // The shape a Builder actually writes: declaration, then `Closes #N`,
    // then `Co-Authored-By:`. git's trailer parser reads only the FINAL
    // paragraph and returns nothing here, so the build would have failed
    // with a message about growth while the real problem was placement.
    let body = "fix: add a guard\n\n\
                    Some prose about why.\n\n\
                    Shell-Budget-Growth: 59 lines — cannot be ported yet (#7758)\n\n\
                    Closes #8154\n\n\
                    Co-Authored-By: Someone <x@y.z>\n";
    let (ok, bad) = parse_growth_declarations(body);
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(ok.len(), 1, "{ok:?}");
    assert_eq!(ok[0].lines, 59);
}

#[test]
fn a_declaration_survives_a_multi_commit_squash() {
    // GitHub's squash of a multi-commit PR concatenates each commit as
    // `* subject` + body, so every declaration lands mid-message. Keying
    // on git's final-paragraph rule would accept the PR and then red-line
    // `main` on the very commit it just approved (#8073/#8105).
    let squashed = "feat: the PR title (#9999)\n\n\
                        * fix: first commit\n\n\
                        Shell-Budget-Growth: 30 lines — part one (#7758)\n\n\
                        * fix: second commit\n\n\
                        Shell-Budget-Growth: 29 lines — part two (#7758)\n\n\
                        ---------\n\n\
                        Co-authored-by: Someone <x@y.z>\n";
    let (ok, bad) = parse_growth_declarations(squashed);
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(ok.len(), 2, "both halves must survive the squash: {ok:?}");
    assert_eq!(ok.iter().map(|d| d.lines).sum::<u64>(), 59);
}

#[test]
fn a_folded_declaration_value_is_rejoined() {
    let body = msg("Shell-Budget-Growth: 12 lines — a reason long enough to\n  wrap (#1)\n");
    let (ok, bad) = parse_growth_declarations(&body);
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(ok.len(), 1);
    assert!(ok[0].reason.contains("wrap"), "{:?}", ok[0]);
    assert_eq!(ok[0].issue, 1);
}

#[test]
fn an_in_place_recategorisation_cannot_launder_portable_growth() {
    // Review's P1: `c.sh` relisted contract -> bootstrap with IDENTICAL
    // line counts, plus 20 brand-new contract lines. The first per-file cut
    // read only the line count, so it saw no loss and admitted it.
    let before = budget_files(&[("c.sh", "contract", 30), ("a.sh", "contract", 0)]);
    let now = budget_files(&[("c.sh", "bootstrap", 30), ("a.sh", "contract", 20)]);
    assert_eq!(
        now.by_file["c.sh"].1, before.by_file["c.sh"].1,
        "the line count is unchanged — only the category moved"
    );
    let d = decl(9999);
    let err = check_against_rev(&now, &before, "base", &GrowthContext { declared: &d })
        .expect_err("a relisted portable file must count as a total loss");
    assert!(err.contains("c.sh  30 -> gone"), "{err}");
}

#[test]
fn a_tab_indented_trailer_is_also_prose() {
    let (ok, bad) = parse_growth_declarations(&msg("\tShell-Budget-Growth: 9 lines — x (#1)\n"));
    assert!(ok.is_empty(), "{ok:?}");
    assert_eq!(bad.len(), 1, "a near-miss must be REPORTED, never ignored");
    assert!(bad[0].why.contains("indented"), "{:?}", bad[0]);
}

/// A budget with per-file detail, which the laundering rule needs.
fn budget_files(files: &[(&str, &str, u64)]) -> Budget {
    let mut b = Budget::default();
    for (path, cat, lines) in files {
        *b.by_category.entry((*cat).to_string()).or_default() += lines;
        *b.files_by_category.entry((*cat).to_string()).or_default() += 1;
        b.by_file
            .insert((*path).to_string(), ((*cat).to_string(), *lines));
    }
    b
}

#[test]
fn a_declaration_cannot_buy_growth_while_portable_shell_is_retired() {
    // Review defeated the first cut of this rule three ways, all without
    // recategorising anything. This is the shape they share: portable
    // shell shrinks in the same change that grows the floor, which makes
    // the category totals identical to an honest "add 20 bootstrap lines".
    //
    // (a) `git mv c.sh d.sh`, list d.sh as bootstrap, add 20 contract lines
    let before = budget_files(&[("c.sh", "contract", 30), ("a.sh", "contract", 0)]);
    let now = budget_files(&[("d.sh", "bootstrap", 30), ("a.sh", "contract", 20)]);
    let d = decl(9999);
    let err = check_against_rev(&now, &before, "base", &GrowthContext { declared: &d })
        .expect_err("a renamed-away portable file must not be buyable");
    assert!(err.contains("c.sh  30 -> gone"), "{err}");

    // (b) the same, with the lines moved in place and NO allowlist change
    let before = budget_files(&[("c.sh", "contract", 30), ("b.sh", "bootstrap", 0)]);
    let now = budget_files(&[("c.sh", "contract", 10), ("b.sh", "bootstrap", 40)]);
    let err = check_against_rev(&now, &before, "base", &GrowthContext { declared: &d })
        .expect_err("a portable file that shrank must not be buyable");
    assert!(err.contains("c.sh  30 -> 10"), "{err}");
    assert!(err.contains("Split the change"), "{err}");
}

#[test]
fn an_honest_floor_addition_is_still_buyable() {
    // The rule must not block what #8154 exists to unblock: adding a safety
    // check to a `vendored` script that cannot be ported. Nothing portable
    // shrinks here.
    let before = budget_files(&[("c.sh", "contract", 30), ("v.sh", "vendored", 500)]);
    let now = budget_files(&[("c.sh", "contract", 30), ("v.sh", "vendored", 559)]);
    let d = decl(59);
    assert!(check_against_rev(&now, &before, "base", &GrowthContext { declared: &d }).is_ok());
}

#[test]
fn retiring_portable_shell_on_its_own_still_passes() {
    // The veto is scoped to the growth path. A pure retirement — the whole
    // point of the epic — must sail through.
    let before = budget_files(&[("c.sh", "contract", 300), ("b.sh", "bootstrap", 50)]);
    let now = budget_files(&[("c.sh", "contract", 10), ("b.sh", "bootstrap", 50)]);
    assert!(now.total() < before.total());
    assert!(check_against_rev(&now, &before, "base", &GrowthContext::none()).is_ok());
}

#[test]
fn two_enormous_declarations_do_not_overflow() {
    let before = budget_with(&[("bootstrap", 50, 1)]);
    let now = budget_with(&[("bootstrap", 60, 1)]);
    let d = vec![
        GrowthDeclaration {
            lines: u64::MAX,
            reason: "a (#1)".into(),
            issue: 1,
        },
        GrowthDeclaration {
            lines: u64::MAX,
            reason: "b (#2)".into(),
            issue: 2,
        },
    ];
    // Debug would panic on a plain sum; release would wrap to a small
    // number and then REFUSE growth it should allow.
    assert!(check_against_rev(&now, &before, "base", &GrowthContext { declared: &d },).is_ok());
}

#[test]
fn a_multibyte_char_at_the_key_boundary_does_not_panic() {
    // Regression: `strip_trailer_prefix` sliced `line[..20]`, and 20 bytes
    // into "docs(cache): measure — …" is the middle of an em-dash, so
    // scanning real history panicked. Every line here has a multi-byte
    // character straddling or near the key's byte length.
    let corpus = "docs(cache): measure — falsify the hypothesis\n\
                      fix: résumé the loop after a rollback — see #1\n\
                      — leading em-dash\n\
                      日本語のコミットメッセージです\n\
                      Shell-Budget-Growth: 9 lines — naïve café (#8154)\n";
    let (ok, bad) = parse_growth_declarations(&msg(corpus));
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(ok.len(), 1, "{ok:?}");
    assert_eq!(ok[0].lines, 9);
    assert_eq!(ok[0].issue, 8154);
}

#[test]
fn unrelated_prose_is_not_mistaken_for_a_declaration() {
    let (ok, bad) = parse_growth_declarations(
            "fix: mention Shell-Budget-Growth: in the docs\n\n             This commit talks about the trailer but does not declare one.\n",
        );
    // The mention is mid-line, so it is not a trailer at all.
    assert!(ok.is_empty(), "{ok:?}");
    assert!(bad.is_empty(), "{bad:?}");
}

// --- git-backed: comparison() resolution ---

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn init_repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().expect("tempdir");
    git(d.path(), &["init", "-q", "-b", "main"]);
    git(d.path(), &["config", "user.email", "t@example.com"]);
    git(d.path(), &["config", "user.name", "t"]);
    std::fs::write(d.path().join("a.txt"), "1\n").expect("write");
    git(d.path(), &["add", "-A"]);
    git(d.path(), &["commit", "-q", "-m", "c1"]);
    d
}

#[test]
fn on_the_base_branch_it_compares_against_the_parent_not_itself() {
    let d = init_repo();
    std::fs::write(d.path().join("b.txt"), "2\n").expect("write");
    git(d.path(), &["add", "-A"]);
    git(d.path(), &["commit", "-q", "-m", "c2"]);
    let c = comparison(d.path(), "main").expect("comparison");
    assert!(c.desc.contains("HEAD~1"), "{}", c.desc);
}

#[test]
fn a_root_commit_with_no_parent_errors_rather_than_comparing_with_itself() {
    // Comparing a tree with itself passes without measuring anything. On a
    // direct push that is a gate nobody is reviewing AND nothing is
    // checking, which is worse than the chore it replaced.
    let d = init_repo();
    let err = comparison(d.path(), "main").expect_err("must refuse");
    assert!(err.contains("no parent"), "{err}");
}

#[test]
fn a_branch_compares_against_the_merge_base_not_the_advanced_tip() {
    let d = init_repo();
    git(d.path(), &["checkout", "-q", "-b", "feature"]);
    std::fs::write(d.path().join("f.txt"), "f\n").expect("write");
    git(d.path(), &["add", "-A"]);
    git(d.path(), &["commit", "-q", "-m", "feature work"]);
    // main advances independently.
    git(d.path(), &["checkout", "-q", "main"]);
    std::fs::write(d.path().join("m.txt"), "m\n").expect("write");
    git(d.path(), &["add", "-A"]);
    git(d.path(), &["commit", "-q", "-m", "main advances"]);
    let base_sha = String::from_utf8_lossy(
        &Command::new("git")
            .arg("-C")
            .arg(d.path())
            .args(["rev-parse", "HEAD~1"])
            .output()
            .expect("git")
            .stdout,
    )
    .trim()
    .to_string();
    git(d.path(), &["checkout", "-q", "feature"]);

    let c = comparison(d.path(), "main").expect("comparison");
    assert_eq!(c.rev, base_sha, "must pick the common ancestor, not main's tip");
}

#[test]
fn an_unresolvable_base_errors_rather_than_measuring_a_different_tree() {
    // Falling back to the base ref looks harmless and is not: it measures a
    // DIFFERENT tree, so a branch that adds shell can report a negative
    // delta and pass. Reproduced in review on a depth-1 clone.
    let d = init_repo();
    let err = comparison(d.path(), "origin/nonexistent").expect_err("must refuse");
    assert!(err.contains("no merge-base"), "{err}");
    assert!(err.contains("fetch-depth"), "must name the usual cause: {err}");
}

#[test]
fn the_production_floor_uses_the_typed_category_not_the_filter_it_guards() {
    // The floor exists to catch `is_production_shell` being wrong, so it
    // must not be computed with it. Review reproduced what happens when it
    // is: narrowing the filter to drop `defaults/` made every gate pass
    // while the report announced "net vs epic start -34222 — retired since
    // the epic began". A fabricated 34,000-line win, green.
    let mut cats = BTreeMap::new();
    cats.insert("defaults/scripts/a.sh".to_string(), "contract".to_string());
    cats.insert("defaults/scripts/b.sh".to_string(), "bootstrap".to_string());
    cats.insert("defaults/scripts/tests/test-a.sh".to_string(), "test".to_string());
    assert_eq!(
        production_entry_count(&cats),
        2,
        "counts the two non-test entries, regardless of what the path filter thinks"
    );
}

#[test]
fn a_narrowed_scope_filter_is_reported_as_a_filter_bug_not_as_progress() {
    let msg = production_filter_error(236, 64);
    assert!(msg.contains("too narrow"), "{msg}");
    assert!(
        msg.contains("manufactures progress that did not happen"),
        "the message must name the consequence, not just the mismatch: {msg}"
    );
}

#[test]
fn the_typed_category_and_the_path_filter_agree_on_the_real_allowlist() {
    // If these ever disagree, one of them is wrong and the floor becomes
    // either slack or a false alarm. Pinning the agreement makes that
    // visible the day it happens rather than the day it matters.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let text =
        std::fs::read_to_string(root.join("scripts/shell-allowlist.txt")).expect("allowlist");
    let cats = parse_allowlist(&text);
    let by_category = production_entry_count(&cats);
    let by_path = cats.keys().filter(|p| is_production_shell(p)).count();
    assert_eq!(
        by_category, by_path,
        "the allowlist's typed categories and is_production_shell disagree about which \
             scripts are production — one of them is wrong"
    );
}

#[test]
fn portable_excludes_the_permanent_floor() {
    let b = budget_of(&[
        ("contract", 100),
        ("hook-entry", 10),
        ("bootstrap", 999),
        ("vendored", 999),
        ("stub", 5),
    ]);
    assert_eq!(b.portable(), 110, "only contract + hook-entry are retirable");
    assert_eq!(b.floor(), 1998);
    assert_eq!(b.total(), 2113);
}

#[test]
fn the_report_says_plainly_when_the_pool_grew() {
    let b = budget_of(&[("contract", 38439), ("bootstrap", 100), ("stub", 145)]);
    let out = render_report(&b, 38374);
    assert!(out.contains("+65"), "{out}");
    assert!(out.contains("has GROWN"), "the direction must not be buried: {out}");
}

#[test]
fn the_report_says_plainly_when_the_pool_shrank() {
    let b = budget_of(&[("contract", 30000), ("stub", 145)]);
    let out = render_report(&b, 38374);
    assert!(out.contains("-8374"), "{out}");
    assert!(out.contains("retired since"), "{out}");
}

#[test]
fn unlisted_scripts_are_surfaced_because_they_make_the_count_wrong() {
    let mut b = budget_of(&[("contract", 10)]);
    b.unlisted
        .push(PathBuf::from("defaults/scripts/mystery.sh"));
    let out = render_report(&b, 10);
    assert!(out.contains("WARNING"), "{out}");
    assert!(out.contains("undercount"), "{out}");
    assert!(out.contains("mystery.sh"), "{out}");
}

#[test]
fn code_lines_matches_the_file_size_ratchets_rule() {
    assert_eq!(code_lines("#!/usr/bin/env bash\n\nset -e\n# note\nfoo\n"), 2);
    assert_eq!(code_lines("   # indented comment\n"), 0);
    assert_eq!(code_lines("  echo hi   # trailing comment\n"), 1);
}

#[test]
fn the_scope_rule_is_not_fooled_by_substrings() {
    assert!(is_production_shell("defaults/scripts/latest/thing.sh"));
    assert!(is_production_shell("defaults/scripts/testable.sh"));
    assert!(!is_production_shell("defaults/scripts/tests/test-x.sh"));
    assert!(!is_production_shell("scripts/test-installer.sh"));
    assert!(!is_production_shell(".loom/hooks/guard.sh"));
}

#[test]
fn an_allowlist_line_needs_both_a_path_and_a_category() {
    let m = parse_allowlist("a.sh contract reason here\n# comment\n\nb.sh\nc.sh bootstrap\n");
    assert_eq!(m.get("a.sh").map(String::as_str), Some("contract"));
    assert_eq!(m.get("c.sh").map(String::as_str), Some("bootstrap"));
    assert!(!m.contains_key("b.sh"), "a path with no category is not an entry");
}

// --- #8237: the `settled` category ---
//
// Three properties make the category a safety valve rather than a loophole,
// and each is pinned here: reclassification is not progress, a settled file may
// shrink but never grow, and a settled file cannot appear from nowhere.

#[test]
fn reclassifying_a_script_as_settled_does_not_improve_net_vs_epic_start() {
    // Constraint 1, and the specific failure the curator located: `net` used to
    // be computed from `portable()` alone, so moving 3,533 lines out of
    // `contract` into a category outside PORTABLE would have reported them as
    // retired for editing one column of the allowlist. If landing #8237 makes
    // the headline look better, it has been built wrong.
    let before = budget_of(&[("contract", 1000), ("bootstrap", 50)]);
    let after = budget_of(&[("contract", 700), ("settled", 300), ("bootstrap", 50)]);

    let origin = 1000;
    let before_report = render_report(&before, origin);
    let after_report = render_report(&after, origin);

    assert_eq!(
        before.comparable(),
        after.comparable(),
        "the comparable pool is invariant under reclassification — that IS the property"
    );
    assert!(before_report.contains("0 — unchanged since the epic began"), "{before_report}");
    assert!(
        after_report.contains("0 — unchanged since the epic began"),
        "moving 300 lines into `settled` must not read as 300 lines retired:\n{after_report}"
    );
}

#[test]
fn retiring_a_settled_script_for_real_still_shows_up_as_progress() {
    // The mirror of the test above, and the reason it is not simply "ignore
    // settled": deleting settled shell is real retirement and must still move
    // the number. A `comparable()` that excluded settled would report nothing.
    let after = budget_of(&[("contract", 700), ("settled", 100), ("bootstrap", 50)]);
    let report = render_report(&after, 1000);
    assert!(report.contains("-200"), "{report}");
    assert!(report.contains("retired since"), "{report}");
}

#[test]
fn the_report_labels_settled_as_descoped_not_as_retired() {
    let b = budget_with(&[("contract", 700, 10), ("settled", 300, 4), ("stub", 20, 1)]);
    let out = render_report(&b, 1000);
    assert!(out.contains("settled (descoped)"), "it needs its own line: {out}");
    assert!(out.contains("descoped"), "{out}");
    assert!(
        out.contains("DESCOPED, not retired"),
        "the distinction is the whole point of the line: {out}"
    );
}

#[test]
fn settled_is_neither_portable_nor_floor() {
    let b = budget_of(&[
        ("contract", 100),
        ("hook-entry", 10),
        ("bootstrap", 999),
        ("vendored", 999),
        ("stub", 5),
        ("settled", 300),
    ]);
    assert_eq!(b.portable(), 110, "settled is not a port target");
    assert_eq!(b.floor(), 1998, "settled is not the permanent floor either");
    assert_eq!(b.settled(), 300);
    assert_eq!(b.comparable(), 410, "portable + settled, and nothing else");
    assert_eq!(b.total(), 2413, "but it is still production shell");
}

#[test]
fn a_settled_file_may_not_grow_and_there_is_no_override() {
    // Constraint 2. Without this the category is a laundering route: an author
    // blocked by the portable ratchet reclassifies the script, then grows it
    // freely behind the OVERRIDABLE total-growth check.
    let before = budget_files(&[("s.sh", "settled", 100), ("b.sh", "bootstrap", 50)]);
    let now = budget_files(&[("s.sh", "settled", 130), ("b.sh", "bootstrap", 50)]);

    let err = check_against_rev(&now, &before, "base", &GrowthContext::none())
        .expect_err("a settled file that grew must be refused");
    assert!(err.contains("s.sh  100 -> 130  (+30)"), "{err}");
    assert!(err.contains("`settled`"), "{err}");

    // And a declaration — which DOES buy floor growth — must not buy this.
    let d = decl(9999);
    let err = check_against_rev(&now, &before, "base", &GrowthContext { declared: &d })
        .expect_err("no override exists for settled growth");
    assert!(err.contains("GROWS shell in the `settled` category"), "{err}");
    assert!(
        err.contains(&format!("no `{GROWTH_TRAILER}` override")),
        "the message must say the override does not apply here: {err}"
    );
}

#[test]
fn reclassify_and_grow_in_one_move_is_refused_too() {
    // The obvious way around a per-CATEGORY ratchet: the category total rises
    // from 0 legitimately when the category is first populated, so the rule has
    // to be per-FILE and has to compare against the file's size at the base
    // WHATEVER category it held there.
    let before = budget_files(&[("s.sh", "contract", 100)]);
    let now = budget_files(&[("s.sh", "settled", 130)]);
    assert!(now.portable() < before.portable(), "the portable leg sees a DROP here");
    let err = check_against_rev(&now, &before, "base", &GrowthContext::none())
        .expect_err("growing a script in the same change that settles it must be refused");
    assert!(err.contains("s.sh  100 -> 130"), "{err}");
}

#[test]
fn a_brand_new_file_cannot_be_born_settled() {
    // `settled` is baseline-only in check-shell-allowlist.sh because zero fixes
    // is vacuous for a file with no history. This is the same rule expressed
    // independently in the ratchet, so neither gate is the only thing standing
    // between a new script and the category.
    let before = budget_files(&[("c.sh", "contract", 100)]);
    let now = budget_files(&[("c.sh", "contract", 100), ("new.sh", "settled", 40)]);
    let err = check_against_rev(&now, &before, "base", &GrowthContext::none())
        .expect_err("a settled file absent at the base must be refused");
    assert!(err.contains("new.sh  new -> 40"), "{err}");
}

#[test]
fn a_settled_file_that_shrinks_is_fine() {
    // "May shrink, not grow" — the shrinking half, which is the whole point of
    // keeping the ratchet on the category at all.
    let before = budget_files(&[("s.sh", "settled", 100), ("c.sh", "contract", 50)]);
    let now = budget_files(&[("s.sh", "settled", 60), ("c.sh", "contract", 50)]);
    assert!(check_against_rev(&now, &before, "base", &GrowthContext::none()).is_ok());
}

#[test]
fn a_settled_file_deleted_outright_is_fine() {
    let before = budget_files(&[("s.sh", "settled", 100), ("c.sh", "contract", 50)]);
    let now = budget_files(&[("c.sh", "contract", 50)]);
    assert!(check_against_rev(&now, &before, "base", &GrowthContext::none()).is_ok());
}

#[test]
fn settling_a_script_does_not_read_as_retiring_it_when_the_floor_grows() {
    // The change that POPULATES the category is exactly this shape: 44 files
    // move `contract` -> `settled` with identical content, while the gate
    // script itself (bootstrap) grows by the checks that enforce the category.
    //
    // Without counting `settled` on the NOW side of the per-file laundering
    // rule, every one of those 44 reads as "100 -> gone" and the declaration
    // path refuses — the feature would be unlandable rather than guarded.
    let before = budget_files(&[
        ("a.sh", "contract", 100),
        ("b.sh", "contract", 80),
        ("gate.sh", "bootstrap", 400),
    ]);
    let now = budget_files(&[
        ("a.sh", "settled", 100),
        ("b.sh", "settled", 80),
        ("gate.sh", "bootstrap", 460),
    ]);
    let d = decl(60);
    check_against_rev(&now, &before, "base", &GrowthContext { declared: &d })
        .expect("a declared floor addition alongside a pure reclassification must land");
}

#[test]
fn moving_lines_out_of_a_settled_file_into_a_portable_one_is_still_refused() {
    // The edge case the test plan names: a settled file that "shrinks" only
    // because its lines moved elsewhere buys nothing. The portable leg fires on
    // the destination.
    let before = budget_files(&[("s.sh", "settled", 100), ("c.sh", "contract", 50)]);
    let now = budget_files(&[("s.sh", "settled", 60), ("c.sh", "contract", 90)]);
    let err = check_against_rev(&now, &before, "base", &GrowthContext::none())
        .expect_err("the lines reappeared in the portable pool");
    assert!(err.contains("PORTABLE"), "{err}");
}

#[test]
fn the_settled_refusal_teaches_the_way_out() {
    // A refusal that does not say what to do instead gets worked around rather
    // than obeyed. The way out of this one is specific: the script is no longer
    // settled, so its entry goes back to `contract` in the same change.
    let before = budget_files(&[("s.sh", "settled", 100)]);
    let now = budget_files(&[("s.sh", "settled", 130)]);
    let err =
        check_against_rev(&now, &before, "base", &GrowthContext::none()).expect_err("must fail");
    assert!(err.contains("back to `contract`"), "{err}");
    assert!(err.contains("MERGE-BASE"), "must say what it compared against: {err}");
}

#[test]
fn the_allowlist_actually_populates_the_category_it_documents() {
    // A category nothing is in is a category nothing checks. This pins that the
    // real manifest carries the initial population, so deleting every entry
    // (and quietly turning the whole feature into dead code) fails a test.
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join("scripts/shell-allowlist.txt"),
    )
    .expect("the manifest is in the repo");
    let n = parse_allowlist(&text)
        .values()
        .filter(|c| *c == SETTLED)
        .count();
    assert!(n >= 20, "expected the settled population, found {n} entries");
}
