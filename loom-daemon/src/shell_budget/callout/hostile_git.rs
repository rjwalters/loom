//! `collect_callout_evidence` under hostile git config (#9529).
//!
//! The evidence parser reads the TEXT of a `git diff`, so any config that
//! changes how git renders a patch used to change the gate's verdict on an
//! otherwise identical change. Each test here measures the same fixture under
//! one such config and requires the answer the default config gives.
//!
//! Every config is written REPO-LOCALLY into the temp fixture. Never `--global`
//! or `--system`: the fleet shares this host, and a wider write would outlive
//! the test and reach every other agent's git.

use super::super::collect_callout_evidence;
use super::*;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.email=shell-budget-test@example.invalid",
            "-c",
            "user.name=Shell Budget Test",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} could not run: {e}"));
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

const SCRIPT: &str = "scripts/a.sh";

const BASE: &str = "\
#!/usr/bin/env bash
set -euo pipefail
echo \"one\"
echo \"two\"
echo \"three\"
echo \"four\"
";

/// `BASE` plus two separate additions: one unrelated line, and — two unchanged
/// lines further down — a call-site. Two unchanged lines is inside the reach of
/// `diff.interHunkContext=3`, which is what makes the hunk-shape test bite.
const CHANGED: &str = "\
#!/usr/bin/env bash
set -euo pipefail
echo \"one\"
_unrelated_growth=1
echo \"two\"
echo \"three\"
if command -v loom-daemon >/dev/null 2>&1; then
    _out=\"$(loom-daemon shell-budget --json)\"
    printf '%s\\n' \"$_out\"
fi
echo \"four\"
";

/// The call-site block's code lines. `_unrelated_growth=1` is NOT one of them.
const CALL_SITE_LINES: u64 = 4;

/// Measure the fixture's call-site evidence with `config` set repo-locally and
/// `attributes` written to `.git/info/attributes`.
fn evidence_under(config: &[(&str, &str)], attributes: Option<&str>) -> u64 {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(root.join("scripts")).expect("mkdir");
    std::fs::write(root.join(SCRIPT), BASE).expect("write base");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    // Uncommitted on purpose: the evidence diff is base-vs-WORKING-TREE.
    std::fs::write(root.join(SCRIPT), CHANGED).expect("write change");

    for (key, value) in config {
        git(root, &["config", "--local", key, value]);
    }
    if let Some(text) = attributes {
        std::fs::write(root.join(".git/info/attributes"), text).expect("write attributes");
    }

    let mut now = Budget::default();
    now.by_file
        .insert(SCRIPT.to_string(), ("contract".to_string(), code_lines(CHANGED) as u64));
    let declared = vec![CalloutDeclaration {
        subcommand: "shell-budget".to_string(),
        lines: CALL_SITE_LINES,
    }];
    let ev = collect_callout_evidence(root, "main", &now, &declared).expect("evidence");
    assert_eq!(ev.len(), 1, "{ev:?}");
    ev[0].lines
}

#[test]
fn the_default_config_credits_the_call_site_and_only_the_call_site() {
    // The control. Every test below must reproduce exactly this figure.
    assert_eq!(evidence_under(&[], None), CALL_SITE_LINES);
}

#[test]
fn a_mnemonic_prefix_does_not_hide_the_call_site() {
    // `+++ w/scripts/a.sh` — the reported case. The path missed `by_file`, so a
    // present and correct call-site was refused as absent.
    assert_eq!(evidence_under(&[("diff.mnemonicPrefix", "true")], None), CALL_SITE_LINES);
}

#[test]
fn a_custom_dst_prefix_does_not_hide_the_call_site() {
    assert_eq!(evidence_under(&[("diff.dstPrefix", "y/")], None), CALL_SITE_LINES);
}

#[test]
fn a_custom_src_prefix_changes_nothing() {
    // A PIN, like `noprefix` below: only the `+++` side is parsed, so this
    // passed before #9529 as well.
    assert_eq!(evidence_under(&[("diff.srcPrefix", "x/")], None), CALL_SITE_LINES);
}

#[test]
fn noprefix_changes_nothing() {
    // A PIN, not a regression test: this passed before #9529 too, through
    // `parse_new_path`'s bare-path fallback. It is here so the explicit
    // `--dst-prefix=b/` is known to win over `diff.noprefix` rather than
    // combine with it.
    assert_eq!(evidence_under(&[("diff.noprefix", "true")], None), CALL_SITE_LINES);
}

#[test]
fn an_external_diff_command_does_not_replace_the_patch() {
    // `diff.external` hands rendering to another program. `true` prints
    // nothing and exits 0, so git reported success with no patch at all.
    assert_eq!(evidence_under(&[("diff.external", "true")], None), CALL_SITE_LINES);
}

#[test]
fn a_textconv_filter_does_not_rewrite_what_is_measured() {
    // A textconv driver makes the hunk bodies the FILTER's output. This one
    // deletes the binary's name, so the call-site stopped being one.
    assert_eq!(
        evidence_under(
            &[("diff.hostile.textconv", "sed -e s/loom-daemon/elsewhere/g")],
            Some("*.sh diff=hostile\n"),
        ),
        CALL_SITE_LINES
    );
}

#[test]
fn inter_hunk_context_does_not_bill_a_neighbouring_line_to_the_callout() {
    // The OVER-credit direction. With `diff.interHunkContext=3` git fuses the
    // unrelated line's hunk into the call-site's, and every added line of a
    // matching hunk is credited — so unrelated portable growth was billed to
    // the callout (5, not 4).
    assert_eq!(evidence_under(&[("diff.interHunkContext", "3")], None), CALL_SITE_LINES);
}

#[test]
fn every_hostile_setting_at_once_still_measures_the_same() {
    assert_eq!(
        evidence_under(
            &[
                ("diff.mnemonicPrefix", "true"),
                ("diff.noprefix", "true"),
                ("diff.srcPrefix", "x/"),
                ("diff.dstPrefix", "y/"),
                ("diff.interHunkContext", "3"),
                ("diff.external", "true"),
                ("diff.hostile.textconv", "sed -e s/loom-daemon/elsewhere/g"),
            ],
            Some("*.sh diff=hostile\n"),
        ),
        CALL_SITE_LINES
    );
}
