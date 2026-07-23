//! Doc-lint test for "Stage -1: Backend detection" in
//! `defaults/.claude/commands/loom/sweep.md` (Issue #3454, Phase D of
//! epic #3449).
//!
//! Phase D adds a new "Stage -1: Backend detection" stage to the sweep
//! skill markdown. The new stage probes whether the loom-daemon is
//! reachable AND whether a multi-account token pool exists; if both
//! preconditions hold and the mode is not C, it dispatches each
//! candidate issue to the daemon via `mcp__loom__dispatch_sweep`
//! (Phase A) and exits. Otherwise it falls through to today's
//! in-process subagent dispatch (Modes A/B/C unchanged).
//!
//! This test grep-checks the markdown file at compile time so that:
//!
//! - The new section header is present (AC #1).
//! - The decision-tree probes (`PROBE_MODE`, `PROBE_DAEMON`,
//!   `PROBE_POOL`, `DECIDE`) are all documented (AC #1 — the contract
//!   text is unambiguous).
//! - The strict-AND precedence between daemon-reachable and
//!   pool-exists is documented (AC #1 — no implicit auto-start
//!   behavior).
//! - The `--no-daemon` opt-out flag is documented in the optional-flags
//!   section AND the new stage section (AC #2).
//! - The 500ms timeout for the daemon Ping probe is documented (AC #2
//!   — the implicit-fallback semantics).
//!
//! If the markdown structure intentionally changes (e.g. a follow-up
//! issue revises the decision tree), update this test together with the
//! markdown so the doc-lint stays in sync with the contract.
//!
//! Companion test: `sweep_md_doc_lint.rs` (Phase B, #3453).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;

const SWEEP_MD_RELATIVE: &str = "../defaults/.claude/commands/loom/sweep.md";

fn read_sweep_md() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(SWEEP_MD_RELATIVE);
    fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "sweep.md not found at {} (CWD-relative path: {}): {e}",
            path.display(),
            SWEEP_MD_RELATIVE,
        );
    })
}

/// AC #1: assert the `## Stage -1: Backend detection` section header
/// is present.
#[test]
fn sweep_md_has_stage_minus_one_section() {
    let content = read_sweep_md();
    assert!(
        content.contains("## Stage -1: Backend detection"),
        "expected `## Stage -1: Backend detection` section in sweep.md — \
         the Phase D backend-detection stage is required by #3454 AC #1"
    );
}

/// AC #1: assert all four probe / decision identifiers are present.
/// These are the exact strings from the decision tree pseudocode the
/// issue body specifies — renames flag drift between the spec and the
/// markdown.
#[test]
fn sweep_md_decision_tree_documents_all_probes() {
    let content = read_sweep_md();
    let required_identifiers: &[&str] = &["PROBE_MODE", "PROBE_DAEMON", "PROBE_POOL", "DECIDE"];
    for id in required_identifiers {
        assert!(
            content.contains(id),
            "sweep.md is missing decision-tree identifier `{id}` — \
             #3454 AC #1 requires the full decision tree be documented \
             (PROBE_MODE / PROBE_DAEMON / PROBE_POOL / DECIDE)"
        );
    }
}

/// AC #1: assert the strict-AND precedence between daemon-reachable
/// and pool-exists is documented. The contract is "either missing →
/// subagent fallback" — this catches accidental relaxations to "OR"
/// or "auto-start daemon if pool exists".
#[test]
fn sweep_md_documents_strict_and_precedence() {
    let content = read_sweep_md();

    // The literal decision-tree precedence string from the issue body.
    assert!(
        content.contains("PROBE_DAEMON AND PROBE_POOL"),
        "sweep.md is missing the literal `PROBE_DAEMON AND PROBE_POOL` \
         conjunction — #3454 AC #1 requires strict-AND precedence \
         (no implicit auto-start, no `OR` fallback to daemon-without-pool)"
    );

    // The prose explanation that either probe failing → subagent.
    // Allow any of several equivalent phrasings to survive editorial
    // changes, but require at least one.
    let strict_and_phrases: &[&str] = &[
        "Strict AND",
        "strict AND",
        "Either missing → subagent",
        "either missing → subagent",
        "Either probe failing → subagent",
    ];
    let found = strict_and_phrases
        .iter()
        .any(|phrase| content.contains(phrase));
    assert!(
        found,
        "sweep.md is missing prose that documents the strict-AND \
         contract (e.g., `Strict AND`, `Either missing → subagent`) — \
         #3454 AC #1 requires the contract be unambiguous"
    );
}

/// AC #1: assert the Mode C short-circuit is documented. Mode C must
/// route to subagent **before** any daemon/pool probe — the daemon
/// does not handle PR-set dispatch in v0.10.0.
#[test]
fn sweep_md_documents_mode_c_short_circuit() {
    let content = read_sweep_md();
    assert!(
        content.contains("if Mode C: use_subagent()"),
        "sweep.md is missing the literal `if Mode C: use_subagent()` \
         line from the decision tree — Mode C must short-circuit to \
         subagent before evaluating PROBE_DAEMON / PROBE_POOL (#3454 AC #1)"
    );
}

/// AC #2: assert the `--no-daemon` flag is documented in both the
/// optional-flags section AND the new stage section.
#[test]
fn sweep_md_documents_no_daemon_flag() {
    let content = read_sweep_md();

    // The flag itself must appear at least three times: once in the
    // optional-flags list, once in the validation rules, and at least
    // once in the new stage section (and the decision tree). Three is
    // a lower bound; the real count is higher.
    let occurrences = content.matches("--no-daemon").count();
    assert!(
        occurrences >= 3,
        "sweep.md mentions `--no-daemon` only {occurrences} time(s); \
         #3454 AC #2 requires the flag be documented in the \
         optional-flags section, the validation rules, and the new \
         Stage -1 section (lower bound: 3 occurrences)"
    );

    // The decision-tree line must mention --no-daemon as a short-circuit
    // ahead of the daemon probe.
    assert!(
        content.contains("elif --no-daemon: use_subagent()")
            || content.contains("elif NO_DAEMON: use_subagent()")
            || (content.contains("--no-daemon") && content.contains("PROBE_DAEMON skipped")),
        "sweep.md does not document `--no-daemon` as a Stage -1 \
         short-circuit ahead of the daemon probe — #3454 AC #2 \
         requires both the explicit opt-out flag AND the implicit \
         fallback"
    );
}

/// AC #2: assert the 500ms timeout on the daemon Ping probe is
/// documented. This is the "implicit fallback" semantic — a stale
/// socket or hung daemon is treated as unavailable after 500ms.
#[test]
fn sweep_md_documents_500ms_daemon_probe_timeout() {
    let content = read_sweep_md();
    assert!(
        content.contains("500ms"),
        "sweep.md is missing the `500ms` daemon probe timeout — \
         #3454 AC #2 requires the implicit fallback (timeout = \
         daemon unavailable) be documented"
    );
}

/// AC #3 & AC #4: assert smoke-test recipes for both backend paths
/// are documented in the new section. The daemon-on path expects
/// sub-2-second exit; the daemon-off path falls through to the
/// existing in-process lifecycle.
#[test]
fn sweep_md_documents_smoke_test_recipes() {
    let content = read_sweep_md();

    // Sub-2-second exit expectation (AC #3 documented expectation).
    assert!(
        content.contains("< 2 seconds")
            || content.contains("sub-2-second")
            || content.contains("Sub-2-second"),
        "sweep.md is missing the sub-2-second exit expectation for the \
         daemon path — #3454 AC #3 requires this be documented"
    );

    // Daemon-off / single-token fallthrough (AC #4).
    assert!(
        content.contains("PROBE_POOL")
            && (content.contains("single-token")
                || content.contains("solo-token")
                || content.contains("< 2 ACCOUNT_KEY_")),
        "sweep.md is missing the daemon-off / single-token \
         fallthrough recipe — #3454 AC #4 requires the no-behavior-\
         change-for-solo-token-operators contract be documented"
    );
}

/// AC #1 / #3: assert the dispatch tool (`mcp__loom__dispatch_sweep`,
/// Phase A) is named as the daemon-path entry point. This anchors the
/// Phase D documentation to the actual Phase A wire protocol.
#[test]
fn sweep_md_references_dispatch_sweep_mcp_tool() {
    let content = read_sweep_md();
    assert!(
        content.contains("mcp__loom__dispatch_sweep"),
        "sweep.md is missing the `mcp__loom__dispatch_sweep` MCP tool \
         reference — #3454 requires the daemon dispatch path consume \
         Phase A's tool (#3452)"
    );
}

/// Regression (#3765): assert the Stage -1 "Resolve auto wave size"
/// snippet does NOT use `mapfile`, a bash-4.0+ builtin that is
/// unavailable in macOS's default `/bin/bash` 3.2. The snippet must
/// capture `loom_wave_size_from_disk`'s two-line stdout with a
/// bash-3.2-portable pattern instead. This guards against
/// reintroduction of any bash-4-only builtin into a documented recipe
/// operators copy-paste into their default shell.
#[test]
fn sweep_md_stage_minus_one_wave_size_snippet_is_bash_3_2_portable() {
    let content = read_sweep_md();
    assert!(
        content.contains("loom_wave_size_from_disk"),
        "sweep.md is missing the `loom_wave_size_from_disk` wave-size \
         resolution snippet — #3765 regression guard cannot anchor to it"
    );
    // Guard against the *invocation* form of the bash-4.0+ array-read
    // builtins (`mapfile -…` / `readarray -…`). A prose mention of the
    // word "mapfile" in an explanatory comment is fine — only executable
    // reintroduction is the regression.
    for bad in ["mapfile -", "readarray -"] {
        assert!(
            !content.contains(bad),
            "sweep.md reintroduced `{bad}…` — a bash-4.0+ builtin \
             unavailable in macOS's default /bin/bash 3.2 (#3765). \
             Capture the helper's two-line stdout with a bash-3.2- \
             portable pattern instead (e.g. command substitution + \
             `sed -n '1p'` / `sed -n '2p'`)."
        );
    }
}

/// AC #1: assert the Limitations table records the Stage -1 row as
/// Implemented (#3454). This is the operator-visible status flip that
/// signals Phase D shipped.
#[test]
fn sweep_md_limitations_table_records_stage_minus_one_implemented() {
    let content = read_sweep_md();
    assert!(
        content.contains("Daemon backend detection") && content.contains("Implemented (#3454"),
        "sweep.md Limitations table is missing the `Daemon backend \
         detection | Implemented (#3454...` row — #3454 AC #1 requires \
         the operator-visible status flip (tolerates appended issue refs, e.g. #3829)"
    );
}
