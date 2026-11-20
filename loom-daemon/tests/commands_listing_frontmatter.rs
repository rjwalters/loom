//! Docs-lint for the `/loom:*` command listing surface (#13960).
//!
//! Every `.md` file under `defaults/.claude/commands/loom/` is a listed skill
//! in each session of an installed repo: Claude Code puts its name +
//! description into context on every request. Reference sub-documents
//! (`builder-pr`, `champion-*` helpers, `sweep-*` detail pages, …) are never
//! entry points — parents load them by file path — so their listing line is
//! pure context cost: 53 of 61 entries on one heavily-used machine had not
//! been dispatched in over a month (#13960).
//!
//! # The rule
//!
//! Claude Code merged commands into skills, and a command file accepts skill
//! frontmatter (except `name`/`paths`). `disable-model-invocation: true`
//! removes the file's description from the model-facing listing — "You can
//! invoke: Yes / Claude can invoke: not on its own / Description not in
//! context, full skill loads when invoked" — while `/loom:<name>` stays
//! user-invocable and a parent's Read-by-path is unaffected.
//!
//! So every command file is classified as exactly one of:
//!
//! - [`ENTRY_POINTS`] — user-invocable role commands; keep their description
//!   in the listing so the model (and autocomplete) can surface them.
//! - [`REFERENCE_SUBDOCS`] — internal reference pages; must carry
//!   `disable-model-invocation: true` frontmatter.
//!
//! A file in neither list fails `every_command_file_is_classified`, so a new
//! command file cannot land without a deliberate classification, and a
//! classification flip without the flag (or vice versa) fails the paired
//! tests. This list mirrors the exclude rules `help.md`'s drift cross-check
//! documents — keep the two in sync when moving a file between classes.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

const COMMANDS_DIR: &str = "../defaults/.claude/commands/loom";

/// User-invocable entry points: `/loom:<name>` a human (or the model, on
/// request) is meant to run. Unchanged by #13960.
const ENTRY_POINTS: &[&str] = &[
    "accelerate",
    "architect",
    "auditor",
    "builder",
    "bump",
    "champion",
    "curator",
    "doctor",
    "driver",
    "epic",
    "guide",
    "help",
    "hermit",
    "imagine",
    "judge",
    "loom",
    "star",
    "sweep",
    "sweep-arguments",
    "watch",
];

/// Reference sub-documents: loaded on demand by a parent command via
/// Read-by-path (see the "Load when" tables in `builder.md`, `sweep.md`,
/// `champion.md`), never an entry point. Same set `help.md`'s drift
/// cross-check excludes.
const REFERENCE_SUBDOCS: &[&str] = &[
    // *-patterns / *-reference
    "architect-patterns",
    "architect-reference",
    "champion-reference",
    "ci-refusals-reference",
    "hermit-patterns",
    "judge-reference",
    "sweep-reference",
    // builder sub-docs
    "builder-complexity",
    "builder-pr",
    "builder-worktree",
    // champion-* helpers (everything but `champion` itself)
    "champion-common",
    "champion-critical-file-hold",
    "champion-epic",
    "champion-epic-guard-invariants",
    "champion-epic-standdown",
    "champion-held-pr-staleness",
    "champion-issue-promo",
    "champion-merge-risk-shadow",
    "champion-premise-false-evidence",
    "champion-pr-merge",
    "champion-ratification-hold",
    "champion-targeted",
    // curator helpers
    "curator-estimate",
    "curator-rate-budget",
    // judge-* on-demand sub-skills (#13957; everything but `judge` itself —
    // `judge-reference` and `judge-ci-red` are listed in their own groups)
    "judge-ci-pending",
    "judge-claims",
    "judge-env-gap",
    "judge-fallback",
    "judge-fast-paths",
    "judge-followups",
    "judge-forge-api",
    "judge-live-verification",
    "judge-merge-conflicts",
    "judge-pr-linking",
    "judge-review-reconciliation",
    "judge-trivial-fixes",
    "judge-verdicts",
    // sweep-* detail pages (the `sweep` / `sweep-arguments` entry points
    // excepted)
    "sweep-backend-detection",
    "sweep-doctor-glm-escalation",
    "sweep-dry-run",
    "sweep-examples",
    "sweep-execution-model",
    "sweep-mode-c-lifecycle",
    "sweep-run-hygiene",
    "sweep-scheduling-signals",
    "sweep-summary-output",
    "sweep-wave-lifecycle",
    // standalone reference pages
    "cargo-target-isolation",
    "comment-body-literal-path",
    "judge-ci-red",
    "orphan-stack-replacement",
    "probe-protocol",
];

fn commands_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(COMMANDS_DIR)
}

fn read_command(name: &str) -> String {
    let path = commands_dir().join(format!("{name}.md"));
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The value of `disable-model-invocation` in the file's leading frontmatter
/// block, if the block exists and sets the field. Boolean spelling per the
/// skills frontmatter reference (true/yes/on/1 in any case).
fn disable_model_invocation(text: &str) -> Option<bool> {
    let mut lines = text.lines();
    if lines.next()?.trim_end_matches('\r') != "---" {
        return None;
    }
    for line in lines {
        match line.trim_end_matches('\r') {
            "---" | "..." => return None,
            _ => {}
        }
        let Some(value) = line.strip_prefix("disable-model-invocation:") else {
            continue;
        };
        return match value.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        };
    }
    None
}

#[test]
fn reference_subdocs_carry_disable_model_invocation() {
    let mut unflagged = Vec::new();
    for name in REFERENCE_SUBDOCS {
        if disable_model_invocation(&read_command(name)) != Some(true) {
            unflagged.push(*name);
        }
    }
    assert!(
        unflagged.is_empty(),
        "reference sub-docs missing `disable-model-invocation: true` frontmatter \
         (they stay in every session's skill listing without it, #13960):\n  {}",
        unflagged.join(", ")
    );
}

#[test]
fn entry_points_stay_model_invocable() {
    let mut flagged = Vec::new();
    for name in ENTRY_POINTS {
        if disable_model_invocation(&read_command(name)) == Some(true) {
            flagged.push(*name);
        }
    }
    assert!(
        flagged.is_empty(),
        "entry points must keep their description in the model-facing listing \
         (#13960 leaves them unchanged):\n  {}",
        flagged.join(", ")
    );
}

#[test]
fn every_command_file_is_classified() {
    let mut problems = Vec::new();
    for entry in fs::read_dir(commands_dir()).expect("commands dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let entry_point = ENTRY_POINTS.contains(&stem);
        let reference = REFERENCE_SUBDOCS.contains(&stem);
        match (entry_point, reference) {
            (true, true) => problems
                .push(format!("{stem}: classified as BOTH entry point and reference sub-doc")),
            (false, false) => problems.push(format!(
                "{stem}: classified as NEITHER — add it to ENTRY_POINTS or REFERENCE_SUBDOCS \
                 in tests/commands_listing_frontmatter.rs (and flag it if reference)"
            )),
            _ => {}
        }
    }
    assert!(problems.is_empty(), "unclassified command files:\n  {}", problems.join("\n  "));
}

/// #13960 deleted the deprecated two-tier-daemon tombstones from
/// `defaults/.claude/commands/loom/` (recorded in
/// `defaults/.loom-retired.list`; `migrate-consumer.sh` removes them from
/// historical installs). A re-add must update the classification lists and
/// this test — a tombstone in the listing is pure context cost.
#[test]
fn deprecated_tombstone_commands_stay_deleted() {
    for name in ["loom-iteration", "loom-parent"] {
        let path = commands_dir().join(format!("{name}.md"));
        assert!(
            !path.exists(),
            "{} reappeared under defaults/.claude/commands/loom/ — it was retired in #13960",
            path.display()
        );
    }
}
