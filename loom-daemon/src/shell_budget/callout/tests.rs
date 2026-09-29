//! Tests for the `Shell-Budget-Callout:` trailer (#9297) — its grammar, the
//! diff evidence behind it, and the four mechanical checks that keep it from
//! being a general override on portable growth.
//!
//! In its own file rather than in `shell_budget/tests.rs` so neither approaches
//! the file-size ratchet's threshold (`.loom/docs/file-size-policy.md`).

use super::*;

/// Wrap a body so its first line is a subject, not a declaration — the
/// positional rule `scan_trailers` enforces.
fn msg(body: &str) -> String {
    format!("subject line\n\n{body}")
}

fn budget_with_files(files: &[(&str, &str)]) -> Budget {
    let mut b = Budget::default();
    for (path, cat) in files {
        b.by_file
            .insert((*path).to_string(), ((*cat).to_string(), 10));
        *b.by_category.entry((*cat).to_string()).or_default() += 10;
        *b.files_by_category.entry((*cat).to_string()).or_default() += 1;
    }
    b
}

// --- grammar ---

#[test]
fn the_documented_shape_parses() {
    let (ok, bad) = parse_callout_declarations(&msg("Shell-Budget-Callout: role-tool-policy +38"));
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(ok.len(), 1);
    assert_eq!(ok[0].subcommand, "role-tool-policy");
    assert_eq!(ok[0].lines, 38);
    assert_eq!(ok[0].top(), "role-tool-policy");
}

#[test]
fn the_separator_and_key_case_are_liberal() {
    for body in [
        "Shell-Budget-Callout: role-tool-policy +38",
        "Shell-Budget-Callout: role-tool-policy 38",
        "Shell-Budget-Callout: role-tool-policy +38 lines for the deny-spec call (#8256)",
        "shell-budget-callout: role-tool-policy +38",
    ] {
        let (ok, bad) = parse_callout_declarations(&msg(body));
        assert!(bad.is_empty(), "{body:?} -> {bad:?}");
        assert_eq!(ok.len(), 1, "{body:?}");
        assert_eq!(ok[0].subcommand, "role-tool-policy", "{body:?}");
        assert_eq!(ok[0].lines, 38, "{body:?}");
    }
}

#[test]
fn a_nested_subcommand_path_keeps_its_top_level_token() {
    let (ok, bad) =
        parse_callout_declarations(&msg("Shell-Budget-Callout: forge check-open-pr +9"));
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(ok[0].subcommand, "forge check-open-pr");
    assert_eq!(ok[0].top(), "forge", "the registry only knows the top level");
    assert_eq!(ok[0].lines, 9);
}

#[test]
fn a_trailer_with_no_count_or_no_subcommand_is_malformed_not_ignored() {
    // Silently degrading to "no declaration" is the #8154 defect: the build
    // then fails with a message about growth while the real problem is a typo.
    for body in [
        "Shell-Budget-Callout: role-tool-policy",
        "Shell-Budget-Callout: +38",
        "Shell-Budget-Callout:",
        "Shell-Budget-Callout: ../evil +3",
    ] {
        let (ok, bad) = parse_callout_declarations(&msg(body));
        assert!(ok.is_empty(), "{body:?} -> {ok:?}");
        assert_eq!(bad.len(), 1, "{body:?}");
    }
}

#[test]
fn placement_rules_are_shared_with_the_growth_trailer() {
    // The whole point of reusing `scan_trailers`: these three cannot drift.
    let indented = msg("\x20   Shell-Budget-Callout: role-tool-policy +38\n");
    let (ok, bad) = parse_callout_declarations(&indented);
    assert!(ok.is_empty());
    assert!(bad[0].why.contains("indented"), "{:?}", bad[0]);

    let fenced = msg("```\nShell-Budget-Callout: role-tool-policy +38\n```\n");
    let (ok, bad) = parse_callout_declarations(&fenced);
    assert!(ok.is_empty());
    assert!(bad[0].why.contains("fenced"), "{:?}", bad[0]);

    let subject = "Shell-Budget-Callout: role-tool-policy +38\n\nbody";
    let (ok, bad) = parse_callout_declarations(subject);
    assert!(ok.is_empty());
    assert!(bad[0].why.contains("SUBJECT"), "{:?}", bad[0]);
}

#[test]
fn the_two_trailers_do_not_see_each_other() {
    // A message carrying both must yield exactly one of each and report
    // NOTHING as malformed — a cross-reported line would tell an author their
    // perfectly good trailer was ignored.
    let body = msg("Shell-Budget-Growth: 12 lines — must stay shell (#7870)\n\
         Shell-Budget-Callout: role-tool-policy +38\n");
    let (callouts, bad) = parse_callout_declarations(&body);
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(callouts.len(), 1);
    assert_eq!(callouts[0].subcommand, "role-tool-policy");

    let (growth, bad) = super::super::parse_growth_declarations(&body);
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(growth.len(), 1);
    assert_eq!(growth[0].lines, 12);
}

// --- diff evidence ---

/// PR #8314's own call-site, reduced: the binary is resolved into a variable,
/// so the line NAMING the subcommand does not itself spell `loom-daemon`.
const PR_8314_HUNK: &str = "\
diff --git a/defaults/scripts/spawn-claude.sh b/defaults/scripts/spawn-claude.sh
--- a/defaults/scripts/spawn-claude.sh
+++ b/defaults/scripts/spawn-claude.sh
@@ -1199,0 +1200,8 @@ unset _loom_print_mode
+# The deny-spec computation lives in `loom-daemon role-tool-policy` (#8322).
+if [[ -n \"${LOOM_ROLE:-}\" ]]; then
+    _loom_policy_bin=\"$(loom_locate_daemon_bin \"$WORKSPACE\")\"
+    _loom_policy_record=\"$(\"$_loom_policy_bin\" role-tool-policy deny-specs --json)\"
+    while IFS= read -r _spec; do
+        _loom_specs+=(\"$_spec\")
+    done < <(jq -r '.specs[]?' <<<\"$_loom_policy_record\")
+fi
";

fn pr_8314_declaration() -> Vec<CalloutDeclaration> {
    vec![CalloutDeclaration {
        subcommand: "role-tool-policy".to_string(),
        lines: 38,
    }]
}

#[test]
fn a_real_call_site_hunk_is_credited_its_code_lines() {
    let now = budget_with_files(&[("defaults/scripts/spawn-claude.sh", "contract")]);
    let ev = measure_evidence(PR_8314_HUNK, &now, &pr_8314_declaration());
    assert_eq!(ev.len(), 1);
    // 8 added lines, one of which is a comment and so is not a code line —
    // the same rule `code_lines` applies to the budget itself.
    assert_eq!(ev[0].lines, 7, "{ev:?}");
}

#[test]
fn a_call_site_in_a_floor_script_earns_nothing() {
    // Growth in `bootstrap` / `vendored` is the `Shell-Budget-Growth:`
    // trailer's business. Crediting it here would let a callout pay for the
    // permanent floor.
    let now = budget_with_files(&[("defaults/scripts/spawn-claude.sh", "bootstrap")]);
    let ev = measure_evidence(PR_8314_HUNK, &now, &pr_8314_declaration());
    assert_eq!(ev[0].lines, 0, "{ev:?}");
}

#[test]
fn a_hunk_that_never_names_the_binary_earns_nothing() {
    let diff = "\
diff --git a/s.sh b/s.sh
+++ b/s.sh
@@ -1,0 +2,2 @@
+role-tool-policy_helper() { :; }
+shell_logic_here
";
    let now = budget_with_files(&[("s.sh", "contract")]);
    let ev = measure_evidence(diff, &now, &pr_8314_declaration());
    assert_eq!(ev[0].lines, 0, "{ev:?}");
}

#[test]
fn a_comment_naming_the_subcommand_is_not_a_call_site() {
    // Prose about a port is not a call to it. Without this, 40 lines of new
    // shell logic plus one `# ported to loom-daemon foo` comment would be
    // credited as a call-site.
    let diff = "\
diff --git a/s.sh b/s.sh
+++ b/s.sh
@@ -1,0 +2,3 @@
+# ported into loom-daemon role-tool-policy, honest
+do_new_shell_logic
+and_more_of_it
";
    let now = budget_with_files(&[("s.sh", "contract")]);
    let ev = measure_evidence(diff, &now, &pr_8314_declaration());
    assert_eq!(ev[0].lines, 0, "{ev:?}");
}

#[test]
fn the_subcommand_must_be_named_as_a_whole_word() {
    let diff = "\
diff --git a/s.sh b/s.sh
+++ b/s.sh
@@ -1,0 +2,2 @@
+# loom-daemon lives here
+run my-role-tool-policy-shim --now
";
    let now = budget_with_files(&[("s.sh", "contract")]);
    let ev = measure_evidence(diff, &now, &pr_8314_declaration());
    assert_eq!(ev[0].lines, 0, "{ev:?}");
}

#[test]
fn a_hunk_is_credited_once_even_when_two_trailers_could_claim_it() {
    let diff = "\
diff --git a/s.sh b/s.sh
+++ b/s.sh
@@ -1,0 +2,2 @@
+loom-daemon alpha --x
+loom-daemon beta --y
";
    let now = budget_with_files(&[("s.sh", "contract")]);
    let declared = vec![
        CalloutDeclaration {
            subcommand: "alpha".into(),
            lines: 5,
        },
        CalloutDeclaration {
            subcommand: "beta".into(),
            lines: 5,
        },
    ];
    let ev = measure_evidence(diff, &now, &declared);
    assert_eq!(ev.iter().map(|e| e.lines).sum::<u64>(), 2, "{ev:?}");
    assert_eq!(ev[0].lines, 2, "first declaration wins the hunk: {ev:?}");
    assert_eq!(ev[1].lines, 0, "{ev:?}");
}

#[test]
fn separate_hunks_in_one_file_accumulate() {
    let diff = "\
diff --git a/s.sh b/s.sh
+++ b/s.sh
@@ -1,0 +2,1 @@
+loom-daemon alpha --x
@@ -40,0 +42,1 @@
+loom-daemon alpha --y
";
    let now = budget_with_files(&[("s.sh", "contract")]);
    let declared = vec![CalloutDeclaration {
        subcommand: "alpha".into(),
        lines: 5,
    }];
    assert_eq!(measure_evidence(diff, &now, &declared)[0].lines, 2);
}

#[test]
fn a_deleted_file_is_not_attributed_to_the_previous_one() {
    // `+++ /dev/null` must clear the current path, or a later hunk would be
    // billed against whichever file happened to come before.
    let diff = "\
diff --git a/s.sh b/s.sh
+++ b/s.sh
@@ -1,0 +2,1 @@
+loom-daemon alpha --x
diff --git a/gone.sh b/gone.sh
+++ /dev/null
@@ -1,1 +0,0 @@
-loom-daemon alpha --gone
";
    let now = budget_with_files(&[("s.sh", "contract")]);
    let declared = vec![CalloutDeclaration {
        subcommand: "alpha".into(),
        lines: 5,
    }];
    assert_eq!(measure_evidence(diff, &now, &declared)[0].lines, 1);
}

// --- the four mechanical checks ---

fn registry() -> Vec<String> {
    ["role-tool-policy", "forge", "alpha"]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

fn evidence_of(subcommand: &str, lines: u64) -> Vec<CalloutEvidence> {
    vec![CalloutEvidence {
        subcommand: subcommand.to_string(),
        lines,
    }]
}

#[test]
fn no_declaration_means_no_allowance_and_no_registry_needed() {
    assert_eq!(allowance(&[], &[], &[]).expect("must succeed"), 0);
}

#[test]
fn a_valid_declaration_is_granted_the_smaller_of_declared_and_measured() {
    let d = pr_8314_declaration();
    assert_eq!(
        allowance(&d, &registry(), &evidence_of("role-tool-policy", 38)).expect("granted"),
        38
    );
    // Over-declaring buys nothing: the grant is a fact about the diff.
    assert_eq!(
        allowance(&d, &registry(), &evidence_of("role-tool-policy", 20)).expect("granted"),
        20
    );
}

#[test]
fn a_subcommand_that_does_not_exist_is_refused() {
    let d = vec![CalloutDeclaration {
        subcommand: "no-such-subcommand".into(),
        lines: 10,
    }];
    let err = allowance(&d, &registry(), &evidence_of("no-such-subcommand", 10))
        .expect_err("must refuse");
    assert!(err.contains("not a `loom-daemon` subcommand"), "{err}");
    assert!(err.contains("no-such-subcommand"), "{err}");
}

#[test]
fn an_unenumerable_registry_refuses_rather_than_waving_it_through() {
    // "Could not check" must never read as "checked and fine".
    let err = allowance(&pr_8314_declaration(), &[], &evidence_of("role-tool-policy", 38))
        .expect_err("must refuse");
    assert!(err.contains("could not enumerate"), "{err}");
}

#[test]
fn a_declaration_above_the_cap_is_refused() {
    let d = vec![CalloutDeclaration {
        subcommand: "role-tool-policy".into(),
        lines: CALLOUT_CAP + 1,
    }];
    let err =
        allowance(&d, &registry(), &evidence_of("role-tool-policy", 999)).expect_err("must refuse");
    assert!(err.contains("exceeds the per-subcommand cap"), "{err}");
    assert!(err.contains(&CALLOUT_CAP.to_string()), "{err}");
}

#[test]
fn a_declaration_exactly_at_the_cap_is_allowed() {
    let d = vec![CalloutDeclaration {
        subcommand: "role-tool-policy".into(),
        lines: CALLOUT_CAP,
    }];
    assert_eq!(
        allowance(&d, &registry(), &evidence_of("role-tool-policy", CALLOUT_CAP)).expect("granted"),
        CALLOUT_CAP
    );
}

#[test]
fn a_declaration_the_diff_shows_no_call_site_for_is_refused() {
    let err = allowance(&pr_8314_declaration(), &registry(), &evidence_of("role-tool-policy", 0))
        .expect_err("must refuse");
    assert!(err.contains("no added line of PORTABLE shell"), "{err}");
}

#[test]
fn two_subcommands_each_get_their_own_capped_allowance() {
    let d = vec![
        CalloutDeclaration {
            subcommand: "alpha".into(),
            lines: 10,
        },
        CalloutDeclaration {
            subcommand: "forge".into(),
            lines: 7,
        },
    ];
    let ev = vec![
        CalloutEvidence {
            subcommand: "alpha".into(),
            lines: 30,
        },
        CalloutEvidence {
            subcommand: "forge".into(),
            lines: 4,
        },
    ];
    assert_eq!(allowance(&d, &registry(), &ev).expect("granted"), 14);
}

#[test]
fn the_cap_is_the_stub_cap() {
    // Kept in step deliberately: a call-site is strictly smaller than the glue
    // a fully ported file leaves behind. If one moves, the other should be
    // argued rather than silently diverge.
    assert_eq!(CALLOUT_CAP, super::super::STUB_CAP as u64);
}

// --- the gate decision, with a callout in the context ---
//
// These exercise `check_against_rev` rather than this module's own functions,
// but they live here rather than in `shell_budget/tests.rs` because that file
// is at the file-size ratchet's threshold (`.loom/docs/file-size-policy.md`)
// and this is the sibling module the ratchet asks new code to go into.

use super::super::{check_against_rev, GrowthContext, GrowthDeclaration};

/// Category totals only — what the portable/floor legs read.
fn budget_with(pairs: &[(&str, u64, u64)]) -> Budget {
    let mut b = Budget::default();
    for (c, lines, files) in pairs {
        b.by_category.insert((*c).to_string(), *lines);
        b.files_by_category.insert((*c).to_string(), *files);
    }
    b
}

/// Per-file detail, which the laundering rule needs.
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

/// A `Shell-Budget-Growth:` declaration, for the non-interaction tests.
fn decl(lines: u64) -> Vec<GrowthDeclaration> {
    super::super::parse_growth_declarations(&format!(
        "fix: add a guard\n\nShell-Budget-Growth: {lines} lines — must stay shell, see (#7870)"
    ))
    .0
}

// --- #9297: declared call-site lines for logic ported into the daemon ---

/// The registry a real run reads out of clap. Fixed here so the unit tests do
/// not depend on which subcommands `loom-daemon` currently happens to have.
fn subcommands() -> Vec<String> {
    ["role-tool-policy", "forge"]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

fn callout(subcommand: &str, lines: u64) -> Vec<CalloutDeclaration> {
    parse_callout_declarations(&format!(
        "feat: port the deny-spec computation\n\nShell-Budget-Callout: {subcommand} +{lines}"
    ))
    .0
}

fn evidence(subcommand: &str, lines: u64) -> Vec<CalloutEvidence> {
    vec![CalloutEvidence {
        subcommand: subcommand.to_string(),
        lines,
    }]
}

#[test]
fn a_declared_call_site_offsets_matching_portable_growth() {
    // PR #8314 in miniature: the deny-spec logic moved into
    // `loom-daemon role-tool-policy`, and the +38 that stayed behind is the
    // call-site. Before #9297 this was refused with no override to reach for.
    let before = budget_with(&[("contract", 100, 2)]);
    let now = budget_with(&[("contract", 138, 2)]);
    let c = callout("role-tool-policy", 38);
    assert_eq!(c.len(), 1, "the fixture must actually declare");
    assert!(check_against_rev(
        &now,
        &before,
        "base",
        &GrowthContext {
            callouts: &c,
            subcommands: &subcommands(),
            evidence: &evidence("role-tool-policy", 38),
            ..GrowthContext::none()
        }
    )
    .is_ok());
}

#[test]
fn the_same_lines_without_a_trailer_are_still_refused() {
    // The regression guard: the carve-out must be opt-in per change, not a
    // relaxation of the default.
    let before = budget_with(&[("contract", 100, 2)]);
    let now = budget_with(&[("contract", 138, 2)]);
    let err =
        check_against_rev(&now, &before, "base", &GrowthContext::none()).expect_err("must fail");
    assert!(err.contains("adds 38 code lines of PORTABLE"), "{err}");
}

#[test]
fn a_trailer_naming_a_subcommand_that_does_not_exist_is_refused() {
    let before = budget_with(&[("contract", 100, 2)]);
    let now = budget_with(&[("contract", 138, 2)]);
    let c = callout("no-such-subcommand", 38);
    let err = check_against_rev(
        &now,
        &before,
        "base",
        &GrowthContext {
            callouts: &c,
            subcommands: &subcommands(),
            evidence: &evidence("no-such-subcommand", 38),
            ..GrowthContext::none()
        },
    )
    .expect_err("must fail");
    // It must fail on the TRAILER, not on the growth: an author whose typo
    // degraded to zero credit would read a message about portable shell and go
    // fix the wrong thing.
    assert!(err.contains("not a `loom-daemon` subcommand"), "{err}");
}

#[test]
fn growth_above_the_declared_call_site_is_still_refused() {
    let before = budget_with(&[("contract", 100, 2)]);
    let now = budget_with(&[("contract", 160, 2)]);
    let c = callout("role-tool-policy", 38);
    let err = check_against_rev(
        &now,
        &before,
        "base",
        &GrowthContext {
            callouts: &c,
            subcommands: &subcommands(),
            evidence: &evidence("role-tool-policy", 38),
            ..GrowthContext::none()
        },
    )
    .expect_err("must fail");
    assert!(err.contains("adds 60 code lines of PORTABLE"), "{err}");
    assert!(err.contains("so the shortfall is 22"), "must net it out: {err}");
}

#[test]
fn a_declaration_above_the_cap_is_refused_by_the_gate() {
    let before = budget_with(&[("contract", 100, 2)]);
    let now = budget_with(&[("contract", 200, 2)]);
    let c = callout("role-tool-policy", CALLOUT_CAP + 1);
    let err = check_against_rev(
        &now,
        &before,
        "base",
        &GrowthContext {
            callouts: &c,
            subcommands: &subcommands(),
            evidence: &evidence("role-tool-policy", 100),
            ..GrowthContext::none()
        },
    )
    .expect_err("must fail");
    assert!(err.contains("exceeds the per-subcommand cap"), "{err}");
}

#[test]
fn a_callout_cannot_pay_for_growth_in_the_permanent_floor() {
    // The laundering shape: retire 38 portable lines, add a 38-line call-site,
    // and quietly grow a `bootstrap` script by 10. Portable nets to zero, so
    // the portable leg never fires — and if the total leg spent the whole
    // declared allowance, the floor's 10 would ride along free.
    let before = budget_files(&[("c.sh", "contract", 100), ("b.sh", "bootstrap", 50)]);
    let now = budget_files(&[("c.sh", "contract", 100), ("b.sh", "bootstrap", 60)]);
    let c = callout("role-tool-policy", 38);
    let err = check_against_rev(
        &now,
        &before,
        "base",
        &GrowthContext {
            callouts: &c,
            subcommands: &subcommands(),
            evidence: &evidence("role-tool-policy", 38),
            ..GrowthContext::none()
        },
    )
    .expect_err("the floor's 10 lines must still be declared");
    assert!(err.contains("permanent floor"), "{err}");
}

#[test]
fn a_callout_does_not_change_the_floor_trailers_behaviour() {
    // The two trailers must not interact. Same floor growth, same
    // `Shell-Budget-Growth:` declaration, with a callout also in play: the
    // outcome is whatever the floor rule alone says.
    let before = budget_files(&[("v.sh", "vendored", 500), ("c.sh", "contract", 100)]);
    let now = budget_files(&[("v.sh", "vendored", 559), ("c.sh", "contract", 100)]);
    let g = decl(59);
    let c = callout("role-tool-policy", 38);
    assert!(check_against_rev(
        &now,
        &before,
        "base",
        &GrowthContext {
            declared: &g,
            callouts: &c,
            subcommands: &subcommands(),
            evidence: &evidence("role-tool-policy", 38),
        }
    )
    .is_ok());

    // And short of it, it still fails by the same amount it would have without
    // the callout — the callout bought nothing here, because nothing portable
    // grew.
    let g = decl(10);
    let err = check_against_rev(
        &now,
        &before,
        "base",
        &GrowthContext {
            declared: &g,
            callouts: &c,
            subcommands: &subcommands(),
            evidence: &evidence("role-tool-policy", 38),
        },
    )
    .expect_err("must fail");
    assert!(err.contains("You declared 10 line(s)"), "{err}");
    assert!(err.contains("49 short"), "{err}");
}

#[test]
fn the_failure_message_describes_the_callout_format_the_code_accepts() {
    // Same pinning as the floor trailer's: the #8154 defect was a message
    // promising a format nothing implemented. Lift the template the gate
    // prints, make it concrete, and require it to both parse AND admit the
    // growth it was printed for.
    let before = budget_with(&[("contract", 100, 2)]);
    let now = budget_with(&[("contract", 138, 2)]);
    let err =
        check_against_rev(&now, &before, "base", &GrowthContext::none()).expect_err("must fail");
    assert!(err.contains(CALLOUT_TRAILER), "message must name the trailer: {err}");

    let line = err
        .lines()
        .find(|l| l.starts_with(CALLOUT_TRAILER))
        .expect("the template must be at column 0, ready to copy");
    let concrete = line
        .replace("<subcommand>", "role-tool-policy")
        .replace("<n>", "38");
    let (ok, bad) = parse_callout_declarations(&format!("subject line\n\n{concrete}"));
    assert!(bad.is_empty(), "the message's own template must parse: {bad:?}");
    assert_eq!(ok.len(), 1, "from {concrete:?}");
    assert_eq!(ok[0].lines, 38);
    assert!(check_against_rev(
        &now,
        &before,
        "base",
        &GrowthContext {
            callouts: &ok,
            subcommands: &subcommands(),
            evidence: &evidence("role-tool-policy", 38),
            ..GrowthContext::none()
        }
    )
    .is_ok());
}
